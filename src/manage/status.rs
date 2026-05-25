//! Read per-session status files written by Claude/OpenCode hooks via the
//! `POST /agent_status` server endpoint. Each file lives at
//! `~/.local/share/ai-pod/agents/{session_id}.json`.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// No hook event has been received for this session yet — we know the
    /// container is up but have no signal from the agent itself.
    Unknown,
    Running,
    Idle,
    AwaitingInput,
    Finished,
}

impl AgentStatus {
    pub fn from_str(s: &str) -> Self {
        match s {
            "Idle" => Self::Idle,
            "AwaitingInput" => Self::AwaitingInput,
            "Finished" => Self::Finished,
            "Running" => Self::Running,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub status: AgentStatus,
    pub status_line: String,
}

#[derive(Deserialize)]
struct StatusFile {
    status: String,
    #[serde(default)]
    status_line: String,
}

/// Read every status file in `agents_dir` and return a session_id → entry map.
/// Returns an empty map if the directory doesn't exist.
pub fn load_all(agents_dir: &PathBuf) -> HashMap<String, StatusEntry> {
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(agents_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if session_id.starts_with('.') {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed: StatusFile = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(_) => continue,
        };
        out.insert(
            session_id,
            StatusEntry {
                status: AgentStatus::from_str(&parsed.status),
                status_line: parsed.status_line,
            },
        );
    }
    out
}
