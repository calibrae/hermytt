use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls_pemfile::{certs, pkcs8_private_keys};
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::TlsAcceptor;

/// Shared TLS configuration loaded from cert/key PEM files.
#[derive(Clone)]
pub struct TlsConfig {
    /// Path to PEM certificate file (kept for axum-server which loads its own).
    pub cert_path: String,
    /// Path to PEM private key file.
    pub key_path: String,
    /// Pre-built acceptor for transports that wrap raw TCP (e.g. TCP transport).
    pub acceptor: TlsAcceptor,
}

impl TlsConfig {
    /// Load TLS config from PEM certificate and private key files.
    pub fn from_pem(cert_path: &str, key_path: &str) -> Result<Self> {
        let cert_file =
            &mut BufReader::new(File::open(Path::new(cert_path)).context("opening TLS cert")?);
        let key_file =
            &mut BufReader::new(File::open(Path::new(key_path)).context("opening TLS key")?);

        let certs: Vec<_> = certs(cert_file)
            .collect::<Result<_, _>>()
            .context("parsing TLS certs")?;
        let key = pkcs8_private_keys(key_file)
            .next()
            .ok_or_else(|| anyhow::anyhow!("no PKCS8 private key found in {}", key_path))?
            .context("parsing TLS private key")?;

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, rustls::pki_types::PrivateKeyDer::Pkcs8(key))
            .context("building TLS server config")?;

        Ok(Self {
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }
}
