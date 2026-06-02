use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/mismosmi/ai-pod/releases/latest";
const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/mismosmi/ai-pod/main/install.sh";

/// File under `~/.ai-pod/` holding the last known latest release version.
const CACHE_FILE: &str = "update-check.json";

/// How long a cached check stays fresh before a background refresh is spawned.
const REFRESH_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Cached result of the most recent GitHub release lookup, persisted to
/// `~/.ai-pod/update-check.json`. The startup notification is rendered from
/// this file so it never has to wait on the network; the file itself is
/// refreshed by a detached background process spawned on launch.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct UpdateCache {
    /// Latest release version (without a leading `v`).
    latest_version: String,
    /// Unix timestamp (seconds) of when the lookup was performed.
    checked_at: u64,
}

pub async fn run_update() -> Result<()> {
    println!("{} {}", "Fetching".blue().bold(), INSTALL_SCRIPT_URL);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("ai-pod/{CURRENT_VERSION}"))
        .build()?;

    let script = client
        .get(INSTALL_SCRIPT_URL)
        .send()
        .await?
        .error_for_status()
        .context("Failed to download install script")?
        .text()
        .await?;

    println!("{}", "Running install script...".blue().bold());

    let mut child = Command::new("bash")
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to spawn bash")?;

    child
        .stdin
        .as_mut()
        .context("Failed to open bash stdin")?
        .write_all(script.as_bytes())
        .context("Failed to write install script to bash")?;

    let status = child.wait().context("Failed to wait for bash")?;

    if !status.success() {
        anyhow::bail!("Install script exited with {status}");
    }

    Ok(())
}

/// Show an update notification from the local cache (no network wait) and, if
/// the cache is missing or stale, spawn a detached background process to
/// refresh it for the next launch. This is the startup entry point and never
/// blocks on the network.
pub fn check_for_update(config_dir: &Path) {
    let path = cache_path(config_dir);
    let cache = read_cache(&path);

    if let Some(ref cache) = cache {
        if is_newer(&cache.latest_version, CURRENT_VERSION) {
            eprintln!(
                "{} {} → {} — {}",
                "Update available:".yellow().bold(),
                CURRENT_VERSION.dimmed(),
                cache.latest_version.green().bold(),
                "https://github.com/mismosmi/ai-pod/releases/latest"
            );
        }
    }

    let stale = cache
        .as_ref()
        .is_none_or(|c| now_secs().saturating_sub(c.checked_at) >= REFRESH_INTERVAL_SECS);
    if stale {
        spawn_background_refresh();
    }
}

/// Fetch the latest release version and write it to the update cache. Invoked
/// by the hidden `fetch-update-cache` subcommand in a detached background
/// process. Failures are silent — a missed refresh just delays the
/// notification to a later launch.
pub async fn fetch_and_cache(config_dir: &Path) {
    if let Ok(latest_version) = fetch_latest_version().await {
        let cache = UpdateCache {
            latest_version,
            checked_at: now_secs(),
        };
        let _ = write_cache(&cache_path(config_dir), &cache);
    }
}

fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join(CACHE_FILE)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(path: &Path, cache: &UpdateCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create config dir for update cache")?;
    }
    let json = serde_json::to_string_pretty(cache)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes()).context("Failed to write update cache")?;
    std::fs::rename(&tmp, path).context("Failed to rename update cache")?;
    Ok(())
}

/// Spawn `ai-pod fetch-update-cache` as a detached background process so the
/// network lookup happens off the startup path. Best-effort: any failure to
/// locate or spawn the executable is ignored.
fn spawn_background_refresh() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("fetch-update-cache")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

async fn fetch_latest_version() -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("ai-pod/{CURRENT_VERSION}"))
        .build()?;

    let resp: serde_json::Value = client
        .get(RELEASES_URL)
        .send()
        .await?
        .json()
        .await?;

    let tag = resp["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tag_name"))?;

    Ok(tag.trim_start_matches('v').to_string())
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut parts = v.splitn(3, '.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cache_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = cache_path(dir.path());
        let cache = UpdateCache {
            latest_version: "1.2.3".into(),
            checked_at: 1_700_000_000,
        };
        write_cache(&path, &cache).unwrap();

        let loaded = read_cache(&path).expect("cache should be readable");
        assert_eq!(loaded.latest_version, "1.2.3");
        assert_eq!(loaded.checked_at, 1_700_000_000);
    }

    #[test]
    fn read_cache_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_cache(&cache_path(dir.path())).is_none());
    }

    #[test]
    fn read_cache_malformed_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = cache_path(dir.path());
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(read_cache(&path).is_none());
    }

    #[test]
    fn write_cache_creates_missing_parent_dir() {
        let dir = TempDir::new().unwrap();
        // config_dir itself does not exist yet — the background refresh may run
        // before `ai-pod init` has created it.
        let config_dir = dir.path().join(".ai-pod");
        let path = cache_path(&config_dir);
        let cache = UpdateCache {
            latest_version: "0.1.0".into(),
            checked_at: 1,
        };
        write_cache(&path, &cache).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn newer_patch() {
        assert!(is_newer("0.2.2", "0.2.1"));
    }

    #[test]
    fn newer_minor() {
        assert!(is_newer("0.3.0", "0.2.9"));
    }

    #[test]
    fn newer_major() {
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn same_version() {
        assert!(!is_newer("0.2.1", "0.2.1"));
    }

    #[test]
    fn older_version() {
        assert!(!is_newer("0.2.0", "0.2.1"));
    }
}
