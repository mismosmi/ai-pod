//! Container launch, persistent home setup, mounts, and workspace cleanup.

mod agent_config;
mod home;
mod launch;
mod lifecycle;
mod mounts;

pub use home::{refresh_skill_in_volume, HOME_VERSION_LABEL};
pub use launch::{launch_container, run_in_container};
pub use lifecycle::{attach_container, clean_container, containers_for_prefix, list_containers};
pub use mounts::remove_mask_volume;
pub(crate) use mounts::resolve_container_target;

use crate::runtime::ContainerRuntime;
use anyhow::{Context, Result};

/// Default home used by generated images and implicit home-relative mounts.
const CONTAINER_HOME: &str = "/home/ai-pod";

pub fn volume_exists(rt: &ContainerRuntime, name: &str) -> Result<bool> {
    let status = rt
        .command()
        .args(["volume", "inspect", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to check if volume exists")?;
    Ok(status.success())
}

fn checked_status(status: std::process::ExitStatus, what: &str) -> Result<()> {
    anyhow::ensure!(status.success(), "{what} failed");
    Ok(())
}
