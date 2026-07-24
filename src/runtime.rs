use anyhow::Result;
use clap::ValueEnum;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::env;
use std::process::Command;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Podman,
    Docker,
}

impl RuntimeKind {
    /// Stable string form. Matches the binary name and is the single source of
    /// truth for the CLI flag, the `AI_POD_RUNTIME` env var, and persisted
    /// session records.
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Docker => "docker",
        }
    }

    /// Parse from a user/config string. Case-insensitive, trims whitespace;
    /// returns `None` for anything unrecognized.
    pub fn from_value(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "podman" => Some(RuntimeKind::Podman),
            "docker" => Some(RuntimeKind::Docker),
            _ => None,
        }
    }

    /// Whether this runtime's binary is present and runnable on PATH.
    pub fn is_available(self) -> bool {
        Command::new(self.as_str())
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

impl FromStr for RuntimeKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_value(s).ok_or_else(|| format!("unknown runtime: {s}"))
    }
}

#[derive(Debug, Clone)]
pub struct ContainerRuntime {
    pub kind: RuntimeKind,
    pub dry_run: bool,
}

impl ContainerRuntime {
    /// Select the container runtime. When `preferred` is set (resolved from the
    /// `--runtime` flag or `AI_POD_RUNTIME` env), that runtime is used and must
    /// be available. When `None`, autodetect: prefer podman, fall back to
    /// docker. Under `dry_run` the availability check is skipped so commands can
    /// be printed on a host without the chosen runtime installed.
    pub fn detect(preferred: Option<RuntimeKind>, dry_run: bool) -> Result<Self> {
        if let Some(kind) = preferred {
            if dry_run || kind.is_available() {
                return Ok(Self { kind, dry_run });
            }
            anyhow::bail!(
                "Requested container runtime `{}` is not available on PATH. \
                 Install it or choose the other runtime.",
                kind.as_str()
            );
        }
        if RuntimeKind::Podman.is_available() {
            return Ok(Self {
                kind: RuntimeKind::Podman,
                dry_run,
            });
        }
        if RuntimeKind::Docker.is_available() {
            return Ok(Self {
                kind: RuntimeKind::Docker,
                dry_run,
            });
        }
        anyhow::bail!(
            "Neither podman nor docker found. Install one of them and ensure it is on your PATH."
        )
    }

    /// The binary name: "podman" or "docker"
    pub fn cmd(&self) -> &'static str {
        self.kind.as_str()
    }

    /// Returns a std::process::Command with the runtime binary.
    /// When `dry_run` is set, returns an `echo` command prefixed with the
    /// runtime name so the intended invocation is printed instead of run.
    pub fn command(&self) -> Command {
        if self.dry_run {
            let mut cmd = Command::new("echo");
            cmd.arg(self.cmd());
            cmd
        } else {
            Command::new(self.cmd())
        }
    }

    /// Returns a tokio::process::Command with the runtime binary.
    /// Honors `dry_run` the same way as `command()`.
    pub fn async_command(&self) -> tokio::process::Command {
        if self.dry_run {
            let mut cmd = tokio::process::Command::new("echo");
            cmd.arg(self.cmd());
            cmd
        } else {
            tokio::process::Command::new(self.cmd())
        }
    }

    /// The hostname that resolves to the host from inside a container.
    pub fn host_gateway(&self) -> &'static str {
        match self.kind {
            RuntimeKind::Podman => "host.containers.internal",
            RuntimeKind::Docker => "host.docker.internal",
        }
    }

    /// The --add-host flag value for host gateway resolution.
    pub fn add_host_arg(&self) -> String {
        format!("--add-host={}:host-gateway", self.host_gateway())
    }

    /// The server URL using the correct gateway hostname.
    pub fn server_url(&self) -> String {
        format!("http://{}:7822", self.host_gateway())
    }

    /// Display name for the runtime (e.g. in generated docs).
    pub fn display_name(&self) -> &'static str {
        match self.kind {
            RuntimeKind::Podman => "Podman",
            RuntimeKind::Docker => "Docker",
        }
    }

    /// On rootless Podman, the default user-namespace mapping remaps the host
    /// user to container UID 0, so pre-existing workspace files appear
    /// root-owned inside the container and the agent hits `EACCES` on its first
    /// write. There's no way to fix that from inside ai-pod without weakening
    /// the namespace boundary, but `PODMAN_USERNS=keep-id` fixes it cleanly.
    /// Detect the situation up front and print a one-line hint instead of
    /// letting the user hit an opaque permission error later.
    ///
    /// Only fires for rootless Podman: the runtime is Podman, it's not a
    /// dry-run, `PODMAN_USERNS` isn't already set, we're not running as root,
    /// and `/etc/subuid` actually has a sub-UID range configured for the current
    /// user (the precondition for rootless UID remapping — this avoids a false
    /// positive for rootful Podman invoked by a non-root user).
    pub fn warn_if_rootless_userns_mismatch(&self) {
        if self.kind != RuntimeKind::Podman || self.dry_run {
            return;
        }
        if env::var_os("PODMAN_USERNS").is_some() {
            return;
        }
        // SAFETY: `getuid` is always safe to call and cannot fail.
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            return;
        }
        let subuid = std::fs::read_to_string("/etc/subuid").ok();
        let username = env::var("USER").ok();
        if !subuid_range_configured(username.as_deref(), uid, subuid.as_deref()) {
            return;
        }
        eprintln!(
            "{} workspace files may appear root-owned inside the container \
             (rootless Podman's default UID mapping).\n  \
             Set {} before running ai-pod to fix this, e.g. `{}`.",
            "warning:".yellow().bold(),
            "PODMAN_USERNS=keep-id".bold(),
            "PODMAN_USERNS=keep-id ai-pod".bold(),
        );
    }
}

/// Whether `/etc/subuid` configures a sub-UID range for the current user,
/// keyed by either the numeric UID or the login name (both forms are valid in
/// `subuid(5)`). A missing/unreadable file (`None`) defaults to `true` so the
/// hint is surfaced rather than silently swallowed on the rootless-Podman hosts
/// this targets.
fn subuid_range_configured(username: Option<&str>, uid: u32, subuid: Option<&str>) -> bool {
    let Some(contents) = subuid else {
        return true;
    };
    let uid_str = uid.to_string();
    contents.lines().any(|line| {
        let field = line.split(':').next().unwrap_or("").trim();
        !field.is_empty() && (field == uid_str || username.is_some_and(|u| field == u))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podman_runtime_properties() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: false,
        };
        assert_eq!(rt.cmd(), "podman");
        assert_eq!(rt.host_gateway(), "host.containers.internal");
        assert_eq!(rt.add_host_arg(), "--add-host=host.containers.internal:host-gateway");
        assert_eq!(rt.server_url(), "http://host.containers.internal:7822");
        assert_eq!(rt.display_name(), "Podman");
    }

    #[test]
    fn docker_runtime_properties() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Docker,
            dry_run: false,
        };
        assert_eq!(rt.cmd(), "docker");
        assert_eq!(rt.host_gateway(), "host.docker.internal");
        assert_eq!(rt.add_host_arg(), "--add-host=host.docker.internal:host-gateway");
        assert_eq!(rt.server_url(), "http://host.docker.internal:7822");
        assert_eq!(rt.display_name(), "Docker");
    }

    #[test]
    fn dry_run_command_echoes_invocation() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: true,
        };
        let output = rt
            .command()
            .args(["run", "--rm", "alpine", "true"])
            .output()
            .expect("echo should execute");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("podman run --rm alpine true"),
            "stdout should contain the full podman invocation, got: {stdout}"
        );
    }

    #[test]
    fn dry_run_off_uses_real_binary() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Docker,
            dry_run: false,
        };
        let program = rt.command().get_program().to_string_lossy().into_owned();
        assert_eq!(program, "docker");
    }

    #[test]
    fn dry_run_on_uses_echo() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: true,
        };
        let program = rt.command().get_program().to_string_lossy().into_owned();
        assert_eq!(program, "echo");
    }

    #[test]
    fn from_value_parses_case_insensitively_and_trims() {
        assert_eq!(RuntimeKind::from_value("podman"), Some(RuntimeKind::Podman));
        assert_eq!(RuntimeKind::from_value("Docker"), Some(RuntimeKind::Docker));
        assert_eq!(
            RuntimeKind::from_value("  PODMAN \n"),
            Some(RuntimeKind::Podman)
        );
        assert_eq!(RuntimeKind::from_value("containerd"), None);
        assert_eq!(RuntimeKind::from_value(""), None);
    }

    #[test]
    fn as_str_round_trips_through_from_value() {
        for kind in [RuntimeKind::Podman, RuntimeKind::Docker] {
            assert_eq!(RuntimeKind::from_value(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn serde_serializes_as_lowercase_binary_name() {
        assert_eq!(
            serde_json::to_string(&RuntimeKind::Podman).unwrap(),
            "\"podman\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeKind::Docker).unwrap(),
            "\"docker\""
        );
        let parsed: RuntimeKind = serde_json::from_str("\"docker\"").unwrap();
        assert_eq!(parsed, RuntimeKind::Docker);
    }

    #[test]
    fn clap_value_variants_match_string_form() {
        // The flag value and the persisted/serde string must be identical so a
        // `--runtime` choice and a stored session record agree.
        for kind in [RuntimeKind::Podman, RuntimeKind::Docker] {
            let pv = kind.to_possible_value().unwrap();
            assert_eq!(pv.get_name(), kind.as_str());
        }
    }

    #[test]
    fn subuid_range_matches_by_username_or_uid() {
        let contents = "alice:100000:65536\n1000:200000:65536\n";
        // Matches by login name.
        assert!(subuid_range_configured(Some("alice"), 4242, Some(contents)));
        // Matches by numeric uid even when the name differs.
        assert!(subuid_range_configured(Some("bob"), 1000, Some(contents)));
    }

    #[test]
    fn subuid_range_absent_for_unlisted_user() {
        let contents = "alice:100000:65536\n";
        assert!(!subuid_range_configured(Some("bob"), 4242, Some(contents)));
        // No username available and uid not listed → not configured.
        assert!(!subuid_range_configured(None, 4242, Some(contents)));
    }

    #[test]
    fn subuid_range_missing_file_defaults_to_warning() {
        // A missing/unreadable /etc/subuid should not suppress the hint on the
        // rootless-Podman hosts this targets.
        assert!(subuid_range_configured(Some("alice"), 1000, None));
    }

    #[test]
    fn warn_userns_noop_for_docker() {
        // Docker does not do rootless UID remapping the way this warns about;
        // the guard clause must return before touching the environment.
        let rt = ContainerRuntime {
            kind: RuntimeKind::Docker,
            dry_run: false,
        };
        rt.warn_if_rootless_userns_mismatch();
    }

    #[test]
    fn warn_userns_noop_in_dry_run() {
        let rt = ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: true,
        };
        rt.warn_if_rootless_userns_mismatch();
    }

    #[test]
    fn detect_honors_explicit_preference_in_dry_run() {
        // dry_run skips the availability probe, so an explicit choice is
        // returned verbatim regardless of what is installed on the host.
        let rt = ContainerRuntime::detect(Some(RuntimeKind::Docker), true).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Docker);
        assert!(rt.dry_run);

        let rt = ContainerRuntime::detect(Some(RuntimeKind::Podman), true).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Podman);
    }
}
