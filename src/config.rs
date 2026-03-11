use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct AppConfig {
    pub config_dir: PathBuf,
    pub runtime_settings: PathBuf,
    pub runtime_claude_md: PathBuf,
    pub home_dir: PathBuf,
}

impl AppConfig {
    pub fn new() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Could not determine home directory")?;
        let config_dir = home_dir.join(".ai-pod");

        Ok(Self {
            runtime_settings: config_dir.join("runtime-settings.json"),
            runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
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

    pub fn claude_settings_path(&self) -> PathBuf {
        self.home_dir.join(".claude").join("settings.json")
    }

    pub fn claude_md_path(&self) -> PathBuf {
        self.home_dir.join(".claude").join("CLAUDE.md")
    }

    /// Returns the directory for storing moved credential files for a given workspace.
    /// E.g., `/home/user/my-project` → `~/.env-files/home-user-my-project/`
    pub fn env_files_project_dir(&self, workspace: &Path) -> PathBuf {
        let slug = workspace
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-");
        self.home_dir.join(".env-files").join(slug)
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
            runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn all_paths_are_under_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(config.runtime_settings.starts_with(&config.config_dir));
        assert!(config.runtime_claude_md.starts_with(&config.config_dir));
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
    fn init_creates_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        assert!(!config.config_dir.exists());
        config.init().unwrap();
        assert!(config.config_dir.exists());
    }
}
