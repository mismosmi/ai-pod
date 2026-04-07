# ai-pod

**Claude Code inside isolated containers — safe, persistent, and project-aware.**

ai-pod manages per-workspace containers that run Claude Code. It works with **Podman** (preferred) or **Docker** — whichever is available on your system. Each workspace gets a dedicated container, a shared background server bridges host interaction, and your personal Claude settings follow you everywhere.

---

## Features

- **Workspace isolation** — each directory gets its own container, named by a hash of its path; projects can't interfere with each other
- **Persistent Claude state** — a named volume preserves `~/.claude` (login, memory, settings) across container restarts
- **Credential scanning** — scans the workspace for secrets before mounting it; prompts you to review or abort
- **Custom Dockerfiles per project** — drop an `ai-pod.Dockerfile` in any project to install extra runtimes, tools, or MCP servers
- **Settings & CLAUDE.md merging** — your host `~/.claude/settings.json` and `CLAUDE.md` are merged with container defaults at launch
- **Host command execution** — the bundled `host-tools` binary lets Claude run commands on the host machine; every command requires your explicit approval with a persistent allowlist
- **Desktop notifications** — a Stop hook fires `host-tools notify-user` when a Claude session ends so you know when to come back
- **Transparent host networking** — containers reach host services at `host.containers.internal` (Podman) or `host.docker.internal` (Docker); no manual port mapping needed
- **Auto-update checks** — silently checks for new releases on startup and notifies you when one is available

---

## Requirements

- [Podman](https://podman.io/) or [Docker](https://www.docker.com/) (Podman is preferred; Docker is used as a fallback if Podman is not found)
- Rust (to build from source)

---

## Installation

### Quick install (Linux & macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/mismosmi/ai-pod/main/install.sh | bash
```

Downloads the latest release binary for your OS and architecture and places it in `~/.local/bin/`.

### Build from source

```sh
cargo install --path .
```

---

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

### Subcommands

| Command | Description |
|---|---|
| `init [--workdir PATH]` | Create an `ai-pod.Dockerfile` in the workspace |
| `build` | Build the container image without launching |
| `list` | List all Claude containers |
| `clean [--workdir PATH]` | Stop and remove the container for a workspace |
| `run <command> [args...]` | Run a command in the container instead of the default |
| `serve` | Start the shared server manually (normally auto-started) |

### Run a specific command in the container

```sh
ai-pod run claude resume   # resume the last Claude session
ai-pod run bash            # open a bash shell in the container
```

---

## Configuration

Your host `~/.claude/CLAUDE.md` and `~/.claude/settings.json` are merged with container defaults at launch time, so your personal Claude preferences carry over automatically.

---

## Per-workspace Dockerfiles

Each workspace can have its own `ai-pod.Dockerfile` that customizes the container image for that project — installing extra runtimes, tools, or MCP servers.

To create one in the current directory:

```sh
ai-pod init
```

This writes an `ai-pod.Dockerfile` to the workspace root based on the default image. Edit it to add anything your project needs (e.g. Node, Python, Playwright, project-specific MCP servers). When `ai-pod` launches, it automatically uses `ai-pod.Dockerfile` if one is present, otherwise it falls back to the global default.

The default image is based on Ubuntu and installs Claude Code via the official install script. The generated Dockerfile includes commented-out examples for common additions like Playwright and MCP servers.

---

## Host interaction

The `host-tools` binary is installed in every container at `/home/claude/.local/bin/host-tools`. Claude is taught to use it via a skill file that is automatically installed in each container.

### host-tools run-command

Run a shell command on the host. The host user is prompted to approve commands not previously allowed. Output streams back in real time.

```sh
host-tools run-command ls ~/Desktop
host-tools run-command open https://example.com
```

List previously approved commands:

```sh
host-tools run-command --list
```

### host-tools notify-user

Send a desktop notification to the host user. The notification title is automatically set to the project name.

```sh
host-tools notify-user "Build finished successfully"
```

A Stop hook already calls this automatically when the Claude session ends.

---

## Security

### Credential scanning

Before mounting your workspace, ai-pod scans for common credential files (`.env`, SSH keys, API token files, etc.) and prompts you to continue or abort. Pass `--no-credential-check` to skip this if you know the workspace is clean.

### Keeping .env files out of the container

Move your `.env` file outside the workspace and symlink it back:

```sh
mkdir -p ~/.env-files/my-project
mv .env ~/.env-files/my-project/.env
ln -s ~/.env-files/my-project/.env .env
```

The symlink target is outside the mount — the container never sees the actual file. Your app still works on the host.

### Host command approval

Claude can only run host commands you have explicitly approved via the interactive prompt. Approved commands are persisted so you only approve each one once.

---

## Marketing website

A static marketing site lives in [`website/index.html`](website/index.html). Open it in any browser — no build step required.
