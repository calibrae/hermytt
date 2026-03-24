use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

/// Persistent storage for Mode 2 paired host keys.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairedHosts {
    pub hosts: HashMap<String, PairedHost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedHost {
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub key: String,
}

/// Decode a pairing token (base64 JSON).
#[derive(Debug, Deserialize)]
pub struct PairingToken {
    pub ip: String,
    pub port: u16,
    pub key: String,
    pub expires: u64,
}

impl PairingToken {
    pub fn decode(token: &str) -> Result<Self> {
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            token.trim(),
        )?;
        let parsed: PairingToken = serde_json::from_slice(&bytes)?;

        // Check expiry.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        anyhow::ensure!(now < parsed.expires, "pairing token expired");

        Ok(parsed)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}:{}/pair", self.ip, self.port)
    }
}

impl PairedHosts {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        // Restrict permissions to owner only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn add(&mut self, host: PairedHost, path: &Path) {
        info!(name = %host.name, ip = %host.ip, "paired host saved");
        self.hosts.insert(host.name.clone(), host);
        if let Err(e) = self.save(path) {
            error!(error = %e, "failed to save paired hosts");
        }
    }

    pub fn remove(&mut self, name: &str, path: &Path) {
        self.hosts.remove(name);
        let _ = self.save(path);
    }
}

/// Get the default path for the keys file (next to config).
pub fn keys_path(config_path: Option<&str>) -> PathBuf {
    match config_path {
        Some(p) => {
            let mut path = PathBuf::from(p);
            path.set_extension("keys.json");
            path
        }
        None => PathBuf::from("hermytt.keys.json"),
    }
}
