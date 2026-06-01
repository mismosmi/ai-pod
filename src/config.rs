use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct AppConfig {
    pub config_dir: PathBuf,
    pub runtime_settings: PathBuf,
    pub home_dir: PathBuf,
}

/// A user-configured host-to-container bind mount applied to every ai-pod
/// container launch. Stored as part of [`GlobalConfig`] in
/// `~/.ai-pod/config.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    /// Tilde-expanded absolute host path, as the user supplied it. Symlinks
    /// are intentionally NOT resolved so users can mount things like a
    /// `~/.claude/skills` directory that is itself a symlink.
    pub host: String,
    /// Explicit container target path, or `None` to mirror under
    /// `/home/ai-pod`. When `None`, the host path must be under the user's
    /// `$HOME` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Read-only by default. Set true via `--writable` on `mount add`.
    #[serde(default)]
    pub writable: bool,
}

/// Global ai-pod configuration shared across all workspaces. Persists to
/// `~/.ai-pod/config.json` with 0o600 permissions.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct GlobalConfig {
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
}

impl GlobalConfig {
    pub fn path(config: &AppConfig) -> PathBuf {
        config.config_dir.join("config.json")
    }

    /// Load `~/.ai-pod/config.json`. Returns default if missing or malformed
    /// (with a stderr warning in the malformed case) so a corrupt file never
    /// blocks a launch.
    pub fn load(config: &AppConfig) -> Self {
        let path = Self::path(config);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "warning: ignoring malformed {}: {}",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let path = Self::path(config);
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .context("Failed to write global config")?;
        file.write_all(json.as_bytes())
            .context("Failed to write global config contents")?;
        std::fs::rename(&tmp, &path).context("Failed to rename global config")?;
        // `.mode(0o600)` only takes effect on O_CREAT; a stale tmp from a
        // crashed earlier run keeps its old mode through truncate. Re-apply
        // explicitly so the rename target is always 0o600.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set permissions on global config")?;
        Ok(())
    }

    /// Returns false if a mount with the same host path already exists.
    pub fn add(&mut self, spec: MountSpec) -> bool {
        if self.mounts.iter().any(|m| m.host == spec.host) {
            return false;
        }
        self.mounts.push(spec);
        true
    }

    /// Returns true if a matching mount was removed.
    pub fn remove(&mut self, host: &str) -> bool {
        let before = self.mounts.len();
        self.mounts.retain(|m| m.host != host);
        before != self.mounts.len()
    }
}

/// Path to a per-session record, relative to the config dir. Kept in a
/// `sessions/` subdirectory so the server's project scanner (which treats every
/// top-level `~/.ai-pod/*.json` stem as a project) never picks these up.
pub fn session_state_path(config_dir: &Path, session_id: &str) -> PathBuf {
    config_dir.join("sessions").join(format!("{session_id}.json"))
}

/// Per-session state persisted to `~/.ai-pod/sessions/{session_id}.json`. The
/// host-side CLI writes this at container launch; the shared server reads it
/// (keyed by the `X-Ai-Pod-Session-Id` header) to run service containers on the
/// same runtime the session was launched with.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionState {
    pub runtime: crate::runtime::RuntimeKind,
}

impl SessionState {
    /// Load a session record straight from a config dir. Returns `None` when the
    /// file is missing or malformed, so callers can fall back gracefully.
    pub fn load_from_dir(config_dir: &Path, session_id: &str) -> Option<Self> {
        let path = session_state_path(config_dir, session_id);
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist this record to `~/.ai-pod/sessions/{session_id}.json` at 0o600,
    /// creating the `sessions/` directory if needed. Atomic via temp + rename.
    pub fn save(&self, config: &AppConfig, session_id: &str) -> Result<()> {
        let dir = config.sessions_dir();
        std::fs::create_dir_all(&dir).context("Failed to create ~/.ai-pod/sessions/")?;
        let path = dir.join(format!("{session_id}.json"));
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .context("Failed to write session state")?;
        file.write_all(json.as_bytes())
            .context("Failed to write session state contents")?;
        std::fs::rename(&tmp, &path).context("Failed to rename session state")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set permissions on session state")?;
        Ok(())
    }
}

impl AppConfig {
    pub fn new() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Could not determine home directory")?;
        let config_dir = home_dir.join(".ai-pod");

        Ok(Self {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir,
        })
    }

    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir).context("Failed to create ~/.ai-pod/")?;
        Ok(())
    }

    /// Returns path to the per-project state file: ~/.ai-pod/{hash}.json
    pub fn project_state_file(&self, hash: &str) -> PathBuf {
        self.config_dir.join(format!("{}.json", hash))
    }

    /// Returns path to the shared server state file: ~/.ai-pod/server.json
    pub fn server_state_file(&self) -> PathBuf {
        self.config_dir.join("server.json")
    }

    /// Directory holding per-session records: ~/.ai-pod/sessions/
    pub fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }

    /// Returns path to a per-session record: ~/.ai-pod/sessions/{session_id}.json
    pub fn session_state_file(&self, session_id: &str) -> PathBuf {
        session_state_path(&self.config_dir, session_id)
    }

    pub fn claude_settings_path(&self) -> PathBuf {
        self.home_dir.join(".claude").join("settings.json")
    }

    pub fn claude_md_path(&self) -> PathBuf {
        self.home_dir.join(".claude").join("CLAUDE.md")
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.home_dir.join(".codex")
    }

    pub fn codex_config_path(&self) -> PathBuf {
        self.codex_dir().join("config.toml")
    }

    pub fn codex_auth_path(&self) -> PathBuf {
        self.codex_dir().join("auth.json")
    }

    /// Returns the directory for storing moved credential files for a given workspace.
    /// E.g., `/home/user/my-project` → `~/.env-files/home-user-my-project/`
    pub fn env_files_project_dir(&self, workspace: &Path) -> PathBuf {
        let canonical =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let slug = canonical
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-");
        self.home_dir.join(".env-files").join(slug)
    }

    #[allow(dead_code)]
    pub fn daemon_log_dir(&self, project_hash: &str) -> PathBuf {
        self.config_dir.join("daemon-logs").join(project_hash)
    }

    #[allow(dead_code)]
    pub fn daemon_log_file(&self, project_hash: &str, daemon_id: &str) -> PathBuf {
        self.daemon_log_dir(project_hash).join(format!("{}.log", daemon_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn all_paths_are_under_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(config.runtime_settings.starts_with(&config.config_dir));
    }

    #[test]
    fn config_dir_is_under_home() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(config.config_dir.starts_with(&config.home_dir));
    }

    #[test]
    fn project_state_file_is_under_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let p = config.project_state_file("abc123def456");
        assert!(p.starts_with(&config.config_dir));
        assert!(p.to_string_lossy().ends_with(".json"));
        assert!(p.to_string_lossy().contains("abc123def456"));
    }

    #[test]
    fn server_state_file_is_under_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let p = config.server_state_file();
        assert!(p.starts_with(&config.config_dir));
        assert!(p.to_string_lossy().ends_with("server.json"));
    }

    #[test]
    fn session_state_file_is_under_sessions_subdir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let p = config.session_state_file("abc123");
        assert!(p.starts_with(config.sessions_dir()));
        assert!(p.to_string_lossy().ends_with("abc123.json"));
        // Must NOT sit directly under config_dir, or the server's project
        // scanner would mistake it for a project state file.
        assert_ne!(p.parent(), Some(config.config_dir.as_path()));
    }

    #[test]
    fn session_state_round_trips_at_0o600() {
        use crate::runtime::RuntimeKind;
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        config.init().unwrap();

        SessionState {
            runtime: RuntimeKind::Docker,
        }
        .save(&config, "sess0001")
        .unwrap();

        let path = config.session_state_file("sess0001");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let loaded = SessionState::load_from_dir(&config.config_dir, "sess0001").unwrap();
        assert_eq!(loaded.runtime, RuntimeKind::Docker);
    }

    #[test]
    fn session_state_load_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(SessionState::load_from_dir(&config.config_dir, "nope").is_none());
    }

    #[test]
    fn claude_settings_path_points_to_settings_json() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let p = config.claude_settings_path();
        assert!(p.ends_with("settings.json"));
        assert!(p.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn claude_md_path_points_to_claude_md() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let p = config.claude_md_path();
        assert!(p.ends_with("CLAUDE.md"));
        assert!(p.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn codex_paths_point_under_codex_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(config.codex_dir().ends_with(".codex"));
        let cfg = config.codex_config_path();
        assert!(cfg.starts_with(config.codex_dir()));
        assert!(cfg.ends_with("config.toml"));
        let auth = config.codex_auth_path();
        assert!(auth.starts_with(config.codex_dir()));
        assert!(auth.ends_with("auth.json"));
    }

    #[test]
    fn init_creates_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(!config.config_dir.exists());
        config.init().unwrap();
        assert!(config.config_dir.exists());
    }

    #[test]
    fn global_config_load_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let loaded = GlobalConfig::load(&config);
        assert!(loaded.mounts.is_empty());
    }

    #[test]
    fn global_config_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        config.init().unwrap();

        let mut gc = GlobalConfig::default();
        assert!(gc.add(MountSpec {
            host: "/home/user/.claude/skills".into(),
            container: None,
            writable: false,
        }));
        assert!(gc.add(MountSpec {
            host: "/etc/secret.pem".into(),
            container: Some("/run/secrets/secret.pem".into()),
            writable: true,
        }));
        gc.save(&config).unwrap();

        let path = GlobalConfig::path(&config);
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "config.json must be 0o600 — may contain references to secret paths"
        );

        let loaded = GlobalConfig::load(&config);
        assert_eq!(loaded.mounts.len(), 2);
        assert_eq!(loaded.mounts[0].host, "/home/user/.claude/skills");
        assert_eq!(loaded.mounts[0].container, None);
        assert!(!loaded.mounts[0].writable);
        assert_eq!(loaded.mounts[1].host, "/etc/secret.pem");
        assert_eq!(
            loaded.mounts[1].container.as_deref(),
            Some("/run/secrets/secret.pem")
        );
        assert!(loaded.mounts[1].writable);
    }

    #[test]
    fn global_config_add_rejects_duplicate_host() {
        let mut gc = GlobalConfig::default();
        let spec = MountSpec {
            host: "/foo".into(),
            container: None,
            writable: false,
        };
        assert!(gc.add(spec.clone()));
        assert!(!gc.add(spec));
        assert_eq!(gc.mounts.len(), 1);
    }

    #[test]
    fn global_config_remove_reports_match() {
        let mut gc = GlobalConfig::default();
        gc.add(MountSpec {
            host: "/foo".into(),
            container: None,
            writable: false,
        });
        assert!(gc.remove("/foo"));
        assert!(!gc.remove("/foo"));
        assert!(gc.mounts.is_empty());
    }

    #[test]
    fn global_config_load_malformed_returns_default() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        config.init().unwrap();
        std::fs::write(GlobalConfig::path(&config), "{not valid json").unwrap();
        let loaded = GlobalConfig::load(&config);
        assert!(loaded.mounts.is_empty());
    }

    #[test]
    fn global_config_save_overwrites_stale_tmp_with_0o600() {
        // Simulate a stale tmp file from a crashed earlier save with
        // permissive bits. The `.mode(0o600)` on OpenOptions only takes
        // effect on O_CREAT, so without the explicit set_permissions after
        // rename the final file would inherit the looser mode.
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        config.init().unwrap();
        let tmp = GlobalConfig::path(&config).with_extension("tmp");
        std::fs::write(&tmp, "stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        let gc = GlobalConfig::default();
        gc.save(&config).unwrap();

        let mode = std::fs::metadata(GlobalConfig::path(&config))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "save must enforce 0o600 even over a stale tmp");
    }
}
