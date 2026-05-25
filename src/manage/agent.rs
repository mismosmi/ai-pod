//! Enumerate running ai-pod containers and merge them with hook-driven status.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::container::{ManagedContainer, list_managed_containers};
use crate::runtime::ContainerRuntime;

use super::status::{AgentStatus, StatusEntry};

/// View-model row backing one entry in the manage TUI's left pane.
#[derive(Clone, Debug)]
pub struct Agent {
    pub container_name: String,
    pub session_id: Option<String>,
    pub workspace_path: Option<String>,
    pub project_name: String,
    pub running: bool,
    pub status: AgentStatus,
    pub status_line: String,
}

impl Agent {
    fn from_container(c: ManagedContainer, entry: Option<&StatusEntry>) -> Self {
        let running = matches!(c.state.as_str(), "running" | "Up" | "up")
            || c.status.to_ascii_lowercase().starts_with("up");
        let project_name = c
            .workspace_path
            .as_deref()
            .and_then(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| c.name.clone());
        let (status, status_line) = if !running {
            (AgentStatus::Finished, "exited".to_string())
        } else if let Some(e) = entry {
            (e.status.clone(), e.status_line.clone())
        } else {
            (AgentStatus::Running, String::new())
        };
        Self {
            container_name: c.name,
            session_id: c.session_id,
            workspace_path: c.workspace_path,
            project_name,
            running,
            status,
            status_line,
        }
    }
}

/// Snapshot the current list of managed containers and join it with the
/// hook-driven status map.
pub fn snapshot(rt: &ContainerRuntime, agents_dir: &PathBuf) -> Result<Vec<Agent>> {
    let containers = list_managed_containers(rt)?;
    let status_map = super::status::load_all(agents_dir);
    let agents = containers
        .into_iter()
        .map(|c| {
            let entry = c.session_id.as_deref().and_then(|sid| status_map.get(sid));
            Agent::from_container(c, entry)
        })
        .collect();
    Ok(agents)
}
