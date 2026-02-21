# ai-pod

A CLI tool that runs Claude Code inside isolated Podman containers, giving each workspace its own persistent container environment.

## How it works

`ai-pod` manages per-workspace Podman containers that run Claude Code. Each workspace gets a dedicated container named by a hash of its path. A background notification server detects when Claude finishes a task and can be used to trigger host-side automations.

- **Workspace isolation** — each directory gets its own container
- **Persistent Claude data** — a named volume preserves `~/.claude` state across sessions (login, settings, memory)
- **Credential scanning** — scans the workspace for secrets before mounting it into a container
- **Host access** — containers can reach host services via `host.containers.internal`
- **Settings & CLAUDE.md merging** — your host `~/.claude/settings.json` and `CLAUDE.md` are merged with container defaults and injected at launch

## Requirements

- [Podman](https://podman.io/)
- Rust (to build from source)

## Installation

### Quick install (Linux & macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/farbenmeer/ai-pod/main/install.sh | bash
```

This downloads the latest release binary for your OS and architecture and places it in `~/.local/bin/`.

### Build from source

```sh
cargo install --path .
```

## Usage

```
ai-pod [OPTIONS] [COMMAND]
```

### Launch Claude in the current directory

```sh
ai-pod
```

### Launch Claude in a specific directory

```sh
ai-pod --workdir /path/to/project
```

### Options

| Flag | Description |
|---|---|
| `--workdir <PATH>` | Use a specific workspace directory (default: cwd) |
| `--rebuild` | Force a rebuild of the container image |
| `--no-credential-check` | Skip scanning the workspace for credential files |
| `--notify-port <PORT>` | Notification server port (default: `9876`) |

### Subcommands

| Command | Description |
|---|---|
| `build` | Build the container image without launching |
| `list` | List all Claude containers |
| `clean [--workdir PATH]` | Stop and remove the container for a workspace |
| `run <command> [args...]` | Run a command in the container instead of the default |
| `stop-server` | Stop the background notification daemon |
| `server-status` | Show notification daemon status |

### Run a specific command in the container

```sh
ai-pod run claude resume   # resume the last Claude session
ai-pod run bash            # open a bash shell in the container
```

The `run` subcommand ensures the container is built and running (creating it if needed), then executes the given command interactively via `podman exec`. All standard flags (`--workdir`, `--rebuild`, etc.) apply.

## Configuration

Your host `~/.claude/CLAUDE.md` and `~/.claude/settings.json` are merged with container defaults at launch time, so your personal Claude preferences carry over automatically.

## Per-workspace Dockerfiles

Each workspace can have its own `ai-pod.Dockerfile` that customizes the container image for that project — installing extra runtimes, tools, or MCP servers.

To create one in the current directory:

```sh
ai-pod init
```

This writes an `ai-pod.Dockerfile` to the workspace root based on the default image. Edit it to add anything your project needs (e.g. Node, Python, Playwright, project-specific MCP servers). When `ai-pod` launches, it automatically uses `ai-pod.Dockerfile` if one is present, otherwise it falls back to the global default.

If no `ai-pod.Dockerfile` exists in the workspace, `ai-pod` will remind you to run `ai-pod init` if you want to customise it.

The default image is based on Ubuntu and installs Claude Code via the official install script. The generated Dockerfile includes commented-out examples for common additions like Playwright and MCP servers.

## How to secure credentials

If you have sensible credentials stored in a .env file in your workspace, an easy way to avoid passing them to claude is to move the .env file somewhere else (`~/.env-files/<workspace-name>`) and symlink them back to the workspace directory (`ln -s ~/.env-files/<workspace-name> .env`).
