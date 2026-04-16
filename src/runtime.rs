use anyhow::Result;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Podman,
    Docker,
}

#[derive(Debug, Clone)]
pub struct ContainerRuntime {
    pub kind: RuntimeKind,
    pub dry_run: bool,
}

impl ContainerRuntime {
    /// Detect which container runtime is available.
    /// Prefers podman; falls back to docker.
    pub fn detect(dry_run: bool) -> Result<Self> {
        if Command::new("podman")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Ok(Self {
                kind: RuntimeKind::Podman,
                dry_run,
            });
        }
        if Command::new("docker")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
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
        match self.kind {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Docker => "docker",
        }
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
}
