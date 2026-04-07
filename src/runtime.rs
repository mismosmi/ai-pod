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
}

impl ContainerRuntime {
    /// Detect which container runtime is available.
    /// Prefers podman; falls back to docker.
    pub fn detect() -> Result<Self> {
        if Command::new("podman")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Ok(Self {
                kind: RuntimeKind::Podman,
            });
        }
        if Command::new("docker")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Ok(Self {
                kind: RuntimeKind::Docker,
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
    pub fn command(&self) -> Command {
        Command::new(self.cmd())
    }

    /// Returns a tokio::process::Command with the runtime binary.
    pub fn async_command(&self) -> tokio::process::Command {
        tokio::process::Command::new(self.cmd())
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
        };
        assert_eq!(rt.cmd(), "docker");
        assert_eq!(rt.host_gateway(), "host.docker.internal");
        assert_eq!(rt.add_host_arg(), "--add-host=host.docker.internal:host-gateway");
        assert_eq!(rt.server_url(), "http://host.docker.internal:7822");
        assert_eq!(rt.display_name(), "Docker");
    }
}
