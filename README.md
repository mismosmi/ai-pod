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
| `stop-server` | Stop the background notification daemon |
| `server-status` | Show notification daemon status |

## Configuration

On first run, `ai-pod` creates `~/.ai-pod/` and writes a default `Dockerfile` there. Customize that file to install additional tools into your Claude containers (e.g. Node, Playwright, MCP servers).

Your host `~/.claude/CLAUDE.md` and `~/.claude/settings.json` are merged with container defaults at launch time, so your personal Claude preferences carry over automatically.

## Container image

The default image (`claude.Dockerfile`) is based on Ubuntu and installs Claude Code via the official install script. Uncomment lines in the Dockerfile to add tools like Playwright or MCP servers.

## How to secure credentials

If you have sensible credentials stored in a .env file in your workspace, an easy way to avoid passing them to claude is to move the .env file somewhere else (`~/.env-files/<workspace-name>`) and symlink them back to the workspace directory (`ln -s ~/.env-files/<workspace-name> .env`).
