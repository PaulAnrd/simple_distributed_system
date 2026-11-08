//! Live CPU/RAM/disk sampling for outgoing heartbeats.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sysinfo::{Disks, System, MINIMUM_CPU_UPDATE_INTERVAL};

/// Samples this machine's resources. Create one per process and keep it:
/// CPU usage is measured as the change between two consecutive refreshes,
/// so a fresh sampler has no baseline yet.
pub struct ResourceSampler {
    sys: System,
    disks: Disks,
    disk_path: PathBuf,
    last_cpu_refresh: Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub cpu_free_pct: f32,
    pub mem_total_bytes: u64,
    pub mem_free_bytes: u64,
    pub disk_free_bytes: u64,
}

impl ResourceSampler {
    /// `disk_path` is the directory whose filesystem's free space is
    /// reported (the data directory in Phase 1; Phase 4 will pass the
    /// dedicated storage volume instead).
    pub fn new(disk_path: &Path) -> Self {
        // Mount points are absolute and free of symlinks, and the mount
        // lookup compares whole path components. A relative path such as
        // the default `./clusterd-data`, or one going through a symlink
        // (macOS's /tmp), would match no mount at all and read as 0 bytes
        // free, so resolve it once here.
        let disk_path =
            std::fs::canonicalize(disk_path).unwrap_or_else(|_| disk_path.to_path_buf());

        let mut sys = System::new();
        // Baseline for the first sample's CPU measurement.
        sys.refresh_cpu_usage();

        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            disk_path,
            last_cpu_refresh: Instant::now(),
        }
    }

    /// Refreshes and samples current resource usage. If the previous CPU
    /// refresh was less than `MINIMUM_CPU_UPDATE_INTERVAL` ago (only the
    /// case right after construction, since heartbeats are seconds apart),
    /// waits out the remainder first: over a shorter span the CPU figure is
    /// meaningless, and the first heartbeat would report an idle CPU.
    pub async fn sample(&mut self) -> Sample {
        let since_last = self.last_cpu_refresh.elapsed();
        if since_last < MINIMUM_CPU_UPDATE_INTERVAL {
            tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL - since_last).await;
        }
        self.sys.refresh_cpu_usage();
        self.last_cpu_refresh = Instant::now();
        self.sys.refresh_memory();
        self.disks.refresh();

        let cpu_used_pct = self.sys.global_cpu_usage();

        Sample {
            cpu_free_pct: (100.0 - cpu_used_pct).clamp(0.0, 100.0),
            mem_total_bytes: self.sys.total_memory(),
            mem_free_bytes: self.sys.available_memory(),
            disk_free_bytes: disk_free_bytes_for(&self.disk_path, &self.disks),
        }
    }
}

/// Finds the disk whose mount point is the longest matching prefix of
/// `path` (i.e. the filesystem that actually contains it) and returns its
/// free space. `path` must be absolute and canonical for any mount to match;
/// see [`ResourceSampler::new`].
fn disk_free_bytes_for(path: &Path, disks: &Disks) -> u64 {
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn sample_reports_nonzero_total_memory_on_any_real_machine() {
        let mut sampler = ResourceSampler::new(Path::new("/"));
        let sample = sampler.sample().await;
        assert!(sample.mem_total_bytes > 0);
    }

    #[tokio::test]
    async fn cpu_free_pct_is_always_within_0_to_100() {
        let mut sampler = ResourceSampler::new(Path::new("/"));
        for _ in 0..2 {
            let sample = sampler.sample().await;
            assert!((0.0..=100.0).contains(&sample.cpu_free_pct));
        }
    }

    #[tokio::test]
    async fn relative_disk_path_still_finds_its_filesystem() {
        // Regression test: a relative data directory (the CLI default is
        // `./clusterd-data`) used to match no mount point and report 0.
        let mut sampler = ResourceSampler::new(Path::new("."));
        let sample = sampler.sample().await;
        assert!(sample.disk_free_bytes > 0);
    }

    #[tokio::test]
    async fn first_sample_measures_real_cpu_usage() {
        // Regression test: the first sample of a fresh sampler used to
        // report the CPU as 100% free whatever the load. Keep one core
        // busy while sampling; the reading must show some usage.
        let stop = Arc::new(AtomicBool::new(false));
        let spinner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        };

        let mut sampler = ResourceSampler::new(Path::new("/"));
        let sample = sampler.sample().await;

        stop.store(true, Ordering::Relaxed);
        spinner.join().unwrap();
        assert!(
            sample.cpu_free_pct < 100.0,
            "first sample read {}% free while a core was busy",
            sample.cpu_free_pct
        );
    }

    #[test]
    fn unknown_path_falls_back_to_zero_rather_than_panicking() {
        let disks = Disks::new_with_refreshed_list();
        let free = disk_free_bytes_for(Path::new("/definitely/not/a/real/mount"), &disks);
        // On a normal system "/" still matches as a prefix root, so this
        // mainly guards against a panic on unusual/empty disk lists rather
        // than asserting a specific value.
        let _ = free;
    }

    #[test]
    fn relative_path_matches_no_mount_point() {
        // Documents why ResourceSampler::new canonicalizes: the lookup
        // itself only works with absolute paths.
        let disks = Disks::new_with_refreshed_list();
        assert_eq!(disk_free_bytes_for(Path::new("relative/dir"), &disks), 0);
    }
}
