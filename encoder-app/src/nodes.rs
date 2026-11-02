//! Persisted list of remote nodes, plus API tokens in the OS keychain.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const LOCAL_ID: &str = "local";
const KEYRING_SERVICE: &str = "EncoderCraft";

/// Everything needed to reconnect to a node except secrets (SSH password, key passphrase,
/// API token), which are never written to this file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: String,
    pub name: String,
    pub host: String,
    pub ssh_port: u16,
    pub username: String,
    /// `None` means password authentication (asked for on each connect).
    pub key_path: Option<PathBuf>,
    /// Pinned SSH host key fingerprint, confirmed by the user at deploy time.
    pub host_key: String,
    pub remote_port: u16,
}

pub struct NodeStore {
    path: PathBuf,
    pub nodes: Vec<NodeRecord>,
}

impl NodeStore {
    pub fn load(path: PathBuf) -> Self {
        let nodes = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, nodes }
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(&self.nodes).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn get(&self, id: &str) -> Option<&NodeRecord> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

pub mod secrets {
    use super::KEYRING_SERVICE;

    fn entry(node_id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, node_id).map_err(|e| e.to_string())
    }

    pub fn set_token(node_id: &str, token: &str) -> Result<(), String> {
        entry(node_id)?.set_password(token).map_err(|e| format!("could not store token in keychain: {e}"))
    }

    pub fn get_token(node_id: &str) -> Result<String, String> {
        entry(node_id)?
            .get_password()
            .map_err(|e| format!("API token not found in keychain ({e}); redeploy the server to issue a new one"))
    }

    pub fn delete_token(node_id: &str) {
        if let Ok(e) = entry(node_id) {
            let _ = e.delete_credential();
        }
    }
}
