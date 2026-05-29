use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
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
