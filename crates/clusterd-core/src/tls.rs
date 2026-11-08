//! Builds rustls configs for mutual TLS between agents, trusting only the
//! cluster's own self-issued CA (see [`crate::identity`]) — not the system
//! trust store. Every node presents a cert and every node verifies the
//! peer's cert; there is no anonymous or server-only-auth mode.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Once};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::error::Error;
use crate::identity::NODE_SAN;

static INSTALL_PROVIDER: Once = Once::new();

/// Installs `ring` as the process-wide default rustls `CryptoProvider`.
/// Idempotent and cheap to call from every entry point that might need TLS
/// (agent, and any CLI command that dials the agent over the network) —
/// rustls panics on config-building calls if no provider was installed.
pub fn install_crypto_provider() {
    INSTALL_PROVIDER.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing rustls ring crypto provider (should only be called once)");
    });
}

/// The fixed identity every clusterd node connects as — see
/// [`crate::identity::NODE_SAN`] for why this is a constant rather than a
/// real hostname.
pub fn node_server_name() -> ServerName<'static> {
    ServerName::try_from(NODE_SAN)
        .expect("NODE_SAN is a fixed valid DNS-label constant")
        .to_owned()
}

pub struct Identity {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

pub fn load_identity(cert_path: &Path, key_path: &Path) -> Result<Identity, Error> {
    let cert_chain = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    Ok(Identity { cert_chain, key })
}

pub fn load_ca_root_store(ca_cert_path: &Path) -> Result<RootCertStore, Error> {
    let mut store = RootCertStore::empty();
    for cert in load_certs(ca_cert_path)? {
        store.add(cert).map_err(Error::Tls)?;
    }
    Ok(store)
}

/// Server-side config for the head's control-plane listener: requires and
/// verifies a client cert signed by the cluster CA, and presents the head's
/// own leaf cert in return.
pub fn server_config(
    identity: Identity,
    ca_roots: RootCertStore,
) -> Result<Arc<ServerConfig>, Error> {
    install_crypto_provider();
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(ca_roots))
        .build()
        .map_err(|e| Error::Tls(rustls::Error::General(e.to_string())))?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(identity.cert_chain, identity.key)
        .map_err(Error::Tls)?;
    Ok(Arc::new(config))
}

/// Client-side config for a worker dialing the head: verifies the head's
/// cert against the cluster CA, and presents the worker's own leaf cert for
/// the head's `WebPkiClientVerifier` to check in return.
pub fn client_config(
    identity: Identity,
    ca_roots: RootCertStore,
) -> Result<Arc<ClientConfig>, Error> {
    install_crypto_provider();
    let config = ClientConfig::builder()
        .with_root_certificates(ca_roots)
        .with_client_auth_cert(identity.cert_chain, identity.key)
        .map_err(Error::Tls)?;
    Ok(Arc::new(config))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let file = File::open(path).map_err(|source| Error::ReadPem {
        what: "certificate",
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|source| Error::ReadPem {
            what: "certificate",
            path: path.to_path_buf(),
            source,
        })?;
    if certs.is_empty() {
        return Err(Error::NoCertInPem {
            path: path.to_path_buf(),
        });
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, Error> {
    let file = File::open(path).map_err(|source| Error::ReadPem {
        what: "private key",
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| Error::ReadPem {
            what: "private key",
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| Error::NoKeyInPem {
            path: path.to_path_buf(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{generate_ca, issue_node_cert};
    use std::io::Write;

    fn write_pem_pair(
        dir: &Path,
        name: &str,
        pair: &crate::identity::PemPair,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert_path = dir.join(format!("{name}.crt"));
        let key_path = dir.join(format!("{name}.key"));
        File::create(&cert_path)
            .unwrap()
            .write_all(pair.cert_pem.as_bytes())
            .unwrap();
        File::create(&key_path)
            .unwrap()
            .write_all(pair.key_pem.as_bytes())
            .unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn builds_matching_server_and_client_configs_from_generated_identities() {
        let ca = generate_ca().unwrap();
        let head_pair = issue_node_cert(&ca, "head").unwrap();
        let worker_pair = issue_node_cert(&ca, "worker-1").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let (ca_cert_path, _) = write_pem_pair(dir.path(), "ca", &ca);
        let (head_cert_path, head_key_path) = write_pem_pair(dir.path(), "head", &head_pair);
        let (worker_cert_path, worker_key_path) =
            write_pem_pair(dir.path(), "worker", &worker_pair);

        let head_identity = load_identity(&head_cert_path, &head_key_path).unwrap();
        let head_roots = load_ca_root_store(&ca_cert_path).unwrap();
        assert!(server_config(head_identity, head_roots).is_ok());

        let worker_identity = load_identity(&worker_cert_path, &worker_key_path).unwrap();
        let worker_roots = load_ca_root_store(&ca_cert_path).unwrap();
        assert!(client_config(worker_identity, worker_roots).is_ok());
    }

    #[test]
    fn missing_cert_file_gives_readable_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.crt");
        match load_certs(&missing) {
            Err(Error::ReadPem { what, .. }) => assert_eq!(what, "certificate"),
            other => panic!("expected ReadPem error, got {other:?}"),
        }
    }

    #[test]
    fn empty_pem_file_gives_no_cert_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.crt");
        File::create(&path).unwrap();
        assert!(matches!(load_certs(&path), Err(Error::NoCertInPem { .. })));
    }
}
