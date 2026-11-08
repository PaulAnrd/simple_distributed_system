use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The live-resource part of a heartbeat, decoupled from the wire
/// [`clusterd_proto::Message::Heartbeat`] shape so this module doesn't need
/// to know about protocol framing at all.
#[derive(Debug, Clone)]
pub struct HeartbeatSnapshot {
    pub cpu_free_pct: f32,
    pub mem_total_bytes: u64,
    pub mem_free_bytes: u64,
    pub disk_free_bytes: u64,
    pub running_jobs: Vec<Uuid>,
    pub agent_version: String,
    pub reported_at: SystemTime,
}

struct NodeEntry {
    node_name: String,
    last_heartbeat: Instant,
    snapshot: HeartbeatSnapshot,
}

/// What `clusterd status` actually renders: a [`NodeEntry`] plus the
/// up/down verdict, computed against a point in time the caller supplies
/// (see [`NodeTable::snapshot_at`]) so it's deterministic to test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeStatus {
    pub node_id: Uuid,
    pub node_name: String,
    pub up: bool,
    pub last_seen_secs_ago: u64,
    pub cpu_free_pct: f32,
    pub mem_total_bytes: u64,
    pub mem_free_bytes: u64,
    pub disk_free_bytes: u64,
    pub running_jobs: usize,
    pub agent_version: String,
}

/// Head-side live view of the pool: which nodes have checked in, with what
/// resources, and how long ago. A node is considered `up` as long as a
/// heartbeat has arrived within `timeout`; nothing here does networking —
/// [`crate::agent`] is the only caller, feeding it heartbeats as they
/// arrive.
pub struct NodeTable {
    nodes: HashMap<Uuid, NodeEntry>,
    timeout: Duration,
}

impl NodeTable {
    pub fn new(timeout: Duration) -> Self {
        Self {
            nodes: HashMap::new(),
            timeout,
        }
    }

    pub fn record_heartbeat(
        &mut self,
        node_id: Uuid,
        node_name: String,
        snapshot: HeartbeatSnapshot,
    ) {
        self.record_heartbeat_at(node_id, node_name, snapshot, Instant::now());
    }

    fn record_heartbeat_at(
        &mut self,
        node_id: Uuid,
        node_name: String,
        snapshot: HeartbeatSnapshot,
        now: Instant,
    ) {
        self.nodes.insert(
            node_id,
            NodeEntry {
                node_name,
                last_heartbeat: now,
                snapshot,
            },
        );
    }

    pub fn snapshot(&self) -> Vec<NodeStatus> {
        self.snapshot_at(Instant::now())
    }

    /// Same as [`Self::snapshot`] but against a caller-supplied "now", so
    /// up/down behavior around the timeout boundary can be tested without
    /// real sleeps.
    pub fn snapshot_at(&self, now: Instant) -> Vec<NodeStatus> {
        let mut statuses: Vec<NodeStatus> = self
            .nodes
            .iter()
            .map(|(node_id, entry)| {
                let elapsed = now.saturating_duration_since(entry.last_heartbeat);
                NodeStatus {
                    node_id: *node_id,
                    node_name: entry.node_name.clone(),
                    up: elapsed <= self.timeout,
                    last_seen_secs_ago: elapsed.as_secs(),
                    cpu_free_pct: entry.snapshot.cpu_free_pct,
                    mem_total_bytes: entry.snapshot.mem_total_bytes,
                    mem_free_bytes: entry.snapshot.mem_free_bytes,
                    disk_free_bytes: entry.snapshot.disk_free_bytes,
                    running_jobs: entry.snapshot.running_jobs.len(),
                    agent_version: entry.snapshot.agent_version.clone(),
                }
            })
            .collect();
        statuses.sort_by(|a, b| a.node_name.cmp(&b.node_name));
        statuses
    }

    pub fn is_known(&self, node_id: Uuid) -> bool {
        self.nodes.contains_key(&node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(disk_free_bytes: u64) -> HeartbeatSnapshot {
        HeartbeatSnapshot {
            cpu_free_pct: 80.0,
            mem_total_bytes: 16_000_000_000,
            mem_free_bytes: 8_000_000_000,
            disk_free_bytes,
            running_jobs: vec![],
            agent_version: "0.1.0".to_string(),
            reported_at: SystemTime::now(),
        }
    }

    #[test]
    fn freshly_heartbeaten_node_is_up() {
        let mut table = NodeTable::new(Duration::from_secs(15));
        let id = Uuid::now_v7();
        let t0 = Instant::now();
        table.record_heartbeat_at(id, "node-a".into(), snapshot(1_000), t0);

        let statuses = table.snapshot_at(t0 + Duration::from_secs(2));
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].up);
        assert_eq!(statuses[0].last_seen_secs_ago, 2);
    }

    #[test]
    fn node_flips_down_exactly_after_timeout_elapses() {
        let mut table = NodeTable::new(Duration::from_secs(15));
        let id = Uuid::now_v7();
        let t0 = Instant::now();
        table.record_heartbeat_at(id, "node-a".into(), snapshot(1_000), t0);

        // At exactly the timeout boundary it's still considered up...
        let statuses = table.snapshot_at(t0 + Duration::from_secs(15));
        assert!(statuses[0].up, "node should still be up at the boundary");

        // ...one tick later, it's down.
        let statuses = table.snapshot_at(t0 + Duration::from_secs(16));
        assert!(!statuses[0].up, "node should be down past the timeout");
    }

    #[test]
    fn later_heartbeat_resets_the_timeout_clock() {
        let mut table = NodeTable::new(Duration::from_secs(15));
        let id = Uuid::now_v7();
        let t0 = Instant::now();
        table.record_heartbeat_at(id, "node-a".into(), snapshot(1_000), t0);
        table.record_heartbeat_at(
            id,
            "node-a".into(),
            snapshot(2_000),
            t0 + Duration::from_secs(10),
        );

        // 16s after the *first* heartbeat, but only 6s after the second.
        let statuses = table.snapshot_at(t0 + Duration::from_secs(16));
        assert!(statuses[0].up);
        assert_eq!(
            statuses[0].disk_free_bytes, 2_000,
            "should reflect the latest snapshot"
        );
    }

    #[test]
    fn multiple_nodes_are_sorted_by_name() {
        let mut table = NodeTable::new(Duration::from_secs(15));
        let t0 = Instant::now();
        table.record_heartbeat_at(Uuid::now_v7(), "zebra".into(), snapshot(1), t0);
        table.record_heartbeat_at(Uuid::now_v7(), "alpha".into(), snapshot(1), t0);
        table.record_heartbeat_at(Uuid::now_v7(), "mid".into(), snapshot(1), t0);

        let statuses = table.snapshot_at(t0);
        let names: Vec<_> = statuses.iter().map(|s| s.node_name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn empty_table_snapshots_to_empty_vec() {
        let table = NodeTable::new(Duration::from_secs(15));
        assert!(table.snapshot().is_empty());
    }

    #[test]
    fn is_known_reflects_recorded_nodes() {
        let mut table = NodeTable::new(Duration::from_secs(15));
        let id = Uuid::now_v7();
        assert!(!table.is_known(id));
        table.record_heartbeat(id, "node-a".into(), snapshot(1));
        assert!(table.is_known(id));
    }
}
