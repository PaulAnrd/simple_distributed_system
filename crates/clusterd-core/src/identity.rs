//! Cluster CA and per-node certificate generation.
//!
//! Trust in this cluster's control protocol is rooted entirely in a small
//! self-issued CA (see [`generate_ca`]) rather than a hand-rolled shared
//! secret: rustls does the actual crypto, we only decide who gets a
//! signed leaf cert. Delivering that leaf cert to a brand-new node rides
//! on Phase 3's SSH-driven `join`, which already gives us an authenticated
//! channel to the machine — one trust mechanism bootstrapping the other.

use std::path::Path;

use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};

use crate::error::Error;

/// Subject alt name every clusterd-issued leaf certificate carries.
/// Authorization here comes entirely from "was this cert signed by our CA",
/// not from hostname/IP matching — a fixed SAN sidesteps DNS/reverse-DNS
/// fragility on a home LAN. Both ends of every connection pin TLS
/// server-name verification to this same constant (see [`crate::tls`]).
pub const NODE_SAN: &str = "clusterd-node";

pub struct PemPair {
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn generate_ca() -> Result<PemPair, Error> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(Error::CertGen)?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = common_name_dn("clusterd cluster CA");

    let key = KeyPair::generate().map_err(Error::CertGen)?;
    let cert = params.self_signed(&key).map_err(Error::CertGen)?;

    Ok(PemPair {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Issues a leaf certificate for `node_name`, signed by the CA whose
/// PEM-encoded cert and key are given. Used both by `clusterd init` (the
/// head issues its own leaf cert right after creating the CA) and by
/// `clusterd cert issue` (issuing a cert for a worker ahead of manually
/// copying it over, until Phase 3 automates that copy).
pub fn issue_node_cert(ca: &PemPair, node_name: &str) -> Result<PemPair, Error> {
    // rcgen has no "load a signing-capable CA handle from PEM" primitive
    // directly; the documented pattern is to reconstruct the CA's params
    // from its existing cert and re-self-sign with the (separately loaded)
    // original key, giving back an equivalent `Certificate` usable as an
    // issuer. The chain of trust that matters lives in the CA cert bytes
    // already written to disk and loaded into the root store elsewhere
    // (see `crate::tls`) — this reconstruction is only used to hand
    // `signed_by` an issuer handle.
    let ca_params = CertificateParams::from_ca_cert_pem(&ca.cert_pem).map_err(Error::CertGen)?;
    let ca_key = KeyPair::from_pem(&ca.key_pem).map_err(Error::CertGen)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(Error::CertGen)?;

    let mut leaf_params =
        CertificateParams::new(vec![NODE_SAN.to_string()]).map_err(Error::CertGen)?;
    leaf_params.distinguished_name = common_name_dn(node_name);

    let leaf_key = KeyPair::generate().map_err(Error::CertGen)?;
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(Error::CertGen)?;

    Ok(PemPair {
        cert_pem: leaf_cert.pem(),
        key_pem: leaf_key.serialize_pem(),
    })
}

fn common_name_dn(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn
}

/// Writes a private key so that it is never readable by anyone but the
/// owner, even for a moment, and even if a file with wider permissions
/// already exists at `path`.
///
/// Opening the existing file with mode `0600` would not be enough: that
/// mode only applies when a file is *created*, so an existing `0644` key
/// file would keep its permissions with the new key inside. Instead the key
/// goes into a brand-new `0600` file next to it, which is then renamed over
/// `path`. The rename also means that a process which already had the old
/// file open never sees the new key.
pub fn write_secret(path: &Path, contents: &str) -> Result<(), Error> {
    let wrap = |source| Error::WriteSecret {
        path: path.to_path_buf(),
        source,
    };

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut tmp_path = path.as_os_str().to_owned();
        tmp_path.push(".tmp");
        let tmp_path = std::path::PathBuf::from(tmp_path);
        // Left over if a previous write was interrupted.
        let _ = std::fs::remove_file(&tmp_path);

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(wrap)?;
        file.write_all(contents.as_bytes()).map_err(wrap)?;
        file.sync_all().map_err(wrap)?;
        std::fs::rename(&tmp_path, path).map_err(wrap)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents).map_err(wrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_cert_is_self_signed_pem() {
        let ca = generate_ca().unwrap();
        assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(ca.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn issued_node_cert_is_signed_and_loadable_by_rustls() {
        let ca = generate_ca().unwrap();
        let leaf = issue_node_cert(&ca, "worker-1").unwrap();
        assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));

        // Confirm the output is real, parseable DER once decoded from PEM —
        // this is what actually matters, since crate::tls consumes it that
        // way, not as opaque strings.
        let mut cursor = std::io::Cursor::new(leaf.cert_pem.as_bytes());
        let certs: Vec<_> = rustls_pemfile::certs(&mut cursor)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn two_ca_calls_produce_different_keys() {
        let ca1 = generate_ca().unwrap();
        let ca2 = generate_ca().unwrap();
        assert_ne!(ca1.key_pem, ca2.key_pem);
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        write_secret(&path, "secret").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_tightens_an_existing_world_readable_file() {
        // Regression test: overwriting an existing key file used to keep
        // its old, wider permissions.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_secret(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode_of(&path), 0o600);
        assert!(!dir.path().join("node.key.tmp").exists());
    }
}
