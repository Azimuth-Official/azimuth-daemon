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
///
/// The fingerprint is derived from `/etc/machine-id` plus the hostname, and a
/// container gets a fresh machine-id every time it is recreated. Refusing a
/// fingerprint mismatch therefore threw away a perfectly good node id on every
/// `docker compose up --force-recreate`, and the daemon re-registered as a new
/// node — the duplicate-node bug. The identity file sits next to the config on
/// the node's own storage, so possessing it is the ownership signal that
/// matters; the fingerprint is only advisory. Adopt the saved id and re-stamp
/// it with the current fingerprint.
pub fn load(config_path: &Path) -> Option<NodeIdentity> {
    let current_fp = machine_fingerprint();
    for path in candidate_paths(config_path) {
        match std::fs::read_to_string(&path) {
            Ok(raw) => match adopt(&raw, &current_fp) {
                Ok(id) => {
                    debug!(path = %path.display(), node_id = %id.node_id, "Loaded node identity");
                    return Some(id);
                }
                Err(reason) => warn!(path = %path.display(), %reason, "Ignoring identity file"),
            },
            Err(_) => continue,
        }
    }
    None
}

/// Decide whether a serialized identity can be adopted on this machine.
/// Split out from `load` so the decision is unit-testable without depending on
/// the machine-scoped identity file that may exist on the developer's host.
fn adopt(raw: &str, current_fp: &str) -> Result<NodeIdentity, String> {
    let id: NodeIdentity =
        serde_json::from_str(raw).map_err(|e| format!("unparseable identity file: {e}"))?;
    if id.node_id.is_empty() {
        return Err("identity file has no node_id".to_string());
    }
    if id.machine_fingerprint != current_fp {
        // A container gets a fresh /etc/machine-id every time it is recreated,
        // so the fingerprint legitimately changes for the same node. Re-stamp
        // rather than discard — discarding is what produced duplicate nodes.
        warn!(
            node_id = %id.node_id,
            "Machine fingerprint changed (container recreated or host re-imaged) — \
             keeping the saved node identity"
        );
        return Ok(NodeIdentity {
            machine_fingerprint: current_fp.to_string(),
            ..id
        });
    }
    Ok(id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_json(node_id: &str, fingerprint: &str) -> String {
        serde_json::to_string_pretty(&NodeIdentity {
            version: 1,
            node_id: node_id.to_string(),
            machine_fingerprint: fingerprint.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .unwrap()
    }

    #[test]
    fn adopts_identity_after_fingerprint_change() {
        // A recreated container gets a fresh /etc/machine-id, so the stored
        // fingerprint will not match. The node id must still be adopted —
        // discarding it is what made the daemon register duplicate nodes.
        let raw = identity_json("11111111-2222-3333-4444-555555555555", &"0".repeat(64));
        let adopted = adopt(&raw, &"a".repeat(64)).expect("must adopt despite fingerprint change");
        assert_eq!(adopted.node_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(
            adopted.machine_fingerprint,
            "a".repeat(64),
            "adopted identity must be re-stamped with the current fingerprint"
        );
    }

    #[test]
    fn keeps_identity_when_fingerprint_matches() {
        let fp = "b".repeat(64);
        let raw = identity_json("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &fp);
        let adopted = adopt(&raw, &fp).expect("matching fingerprint must load");
        assert_eq!(adopted.node_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(adopted.machine_fingerprint, fp);
    }

    #[test]
    fn rejects_identity_without_node_id() {
        let raw = identity_json("", &"f".repeat(64));
        assert!(
            adopt(&raw, &"f".repeat(64)).is_err(),
            "an empty node_id is not a usable identity"
        );
    }

    #[test]
    fn rejects_unparseable_identity() {
        assert!(adopt("{not json", &"c".repeat(64)).is_err());
    }
}
