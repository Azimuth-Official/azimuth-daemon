//! Machine-scoped node identity persistence.
//!
//! The identity file survives app uninstall/reinstall so a machine keeps its
//! node_id instead of registering a duplicate node each time. Stored outside
//! the install dir and outside per-user config:
//!   Windows: %ProgramData%\Azimuth\node-identity.json
//!   Unix:    /var/lib/azimuth/node-identity.json
//! Falls back to the config file's directory when the primary location is not
//! writable (logged at WARN — two identity paths on one machine can reintroduce
//! duplicate registrations).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

pub const IDENTITY_FILE: &str = "node-identity.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub version: u32,
    pub node_id: String,
    pub machine_fingerprint: String,
    pub created_at: String,
}

fn primary_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("Azimuth"))
    }
    #[cfg(unix)]
    {
        Some(PathBuf::from("/var/lib/azimuth"))
    }
}

fn candidate_paths(config_path: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = primary_dir() {
        v.push(d.join(IDENTITY_FILE));
    }
    if let Some(parent) = config_path.parent() {
        v.push(parent.join(IDENTITY_FILE));
    }
    v
}

/// Stable machine fingerprint: SHA-256 of a machine GUID plus hostname.
/// Raw hardware identifiers never leave the machine.
pub fn machine_fingerprint() -> String {
    let guid = machine_guid().unwrap_or_default();
    let host = hostname().unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(guid.as_bytes());
    hasher.update(b"|");
    hasher.update(host.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(windows)]
fn machine_guid() -> Option<String> {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.contains("MachineGuid"))
        .and_then(|l| l.split_whitespace().last())
        .map(|s| s.to_string())
}

#[cfg(unix)]
fn machine_guid() -> Option<String> {
    std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn hostname() -> Option<String> {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
    #[cfg(unix)]
    {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
    }
}

/// Load the persisted identity, preferring the machine-scoped location.
/// Only returns an identity whose fingerprint matches this machine.
pub fn load(config_path: &Path) -> Option<NodeIdentity> {
    let current_fp = machine_fingerprint();
    for path in candidate_paths(config_path) {
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<NodeIdentity>(&raw) {
                Ok(id) => {
                    if id.machine_fingerprint == current_fp && !id.node_id.is_empty() {
                        debug!(path = %path.display(), "Loaded node identity");
                        return Some(id);
                    }
                    warn!(
                        path = %path.display(),
                        "Identity file fingerprint does not match this machine — ignoring"
                    );
                }
                Err(e) => warn!(path = %path.display(), error = %e, "Unparseable identity file"),
            },
            Err(_) => continue,
        }
    }
    None
}

/// Persist the identity. Tries the machine-scoped dir first, falls back to the
/// config dir (WARN: path divergence risk).
pub fn save(config_path: &Path, node_id: &str) {
    let identity = NodeIdentity {
        version: 1,
        node_id: node_id.to_string(),
        machine_fingerprint: machine_fingerprint(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let json = match serde_json::to_string_pretty(&identity) {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, "Failed to serialize node identity");
            return;
        }
    };
    let paths = candidate_paths(config_path);
    for (i, path) in paths.iter().enumerate() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, &json) {
            Ok(()) => {
                if i > 0 {
                    warn!(
                        path = %path.display(),
                        "Node identity saved to FALLBACK location — machine-scoped dir was not writable; \
                         identity may diverge between install variants on this machine"
                    );
                } else {
                    debug!(path = %path.display(), "Node identity saved");
                }
                return;
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Could not write identity file, trying fallback");
            }
        }
    }
    warn!("Node identity could NOT be persisted anywhere — duplicate registration possible after reinstall");
}
