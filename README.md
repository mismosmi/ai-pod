# ai-pod

[Read the docs](https://ai-pod.apps.farbenmeer.de)

**Claude Code & OpenCode inside isolated containers — safe, persistent, and project-aware.**

ai-pod manages per-workspace containers that run Claude Code or OpenCode. It works with **Podman** (preferred) or **Docker** — whichever is available on your system. Each workspace gets a dedicated container, a shared background server bridges host interaction via MCP, and your personal agent settings follow you everywhere.

---

## Features

- **Workspace isolation** — each directory gets its own container, named by a hash of its path; projects can't interfere with each other
- **Persistent agent state** — a named volume preserves `~/.claude` and `~/.config/opencode` (login, memory, settings) across container restarts
- **Credential scanning** — scans the workspace for secrets before mounting it; prompts you to review or abort
- **Custom Dockerfiles per project** — drop an `ai-pod.Dockerfile` in any project to install extra runtimes, tools, or MCP servers
- **AI-driven skill file** — container environment context and host-command usage are delivered via an auto-generated ai-pod skill loaded by Claude and OpenCode
- **Host command execution via MCP** — the in-container agent talks to the shared host server over MCP (`http://host.containers.internal:7822/mcp`); every host command requires your explicit approval with a persistent allowlist
- **File-based command output** — every command writes stdout/stderr/exit to `{workspace}/.ai-pod/commands/{session_id}/{command_id}/` so the agent reads long-running output directly
- **Interactive TUIs** — `ai-pod commands` to inspect/kill running host commands, `ai-pod allowed` to manage the whitelist
- **Desktop notifications** — Stop hooks notify you on Claude session end, and an OpenCode plugin sends notifications when `session.idle` fires
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

### Launch the agent in the current directory

```sh
ai-pod
```

### Launch in a specific directory

```sh
ai-pod --workdir /path/to/project
```

### Options

| Flag | Description |
|---|---|
| `--workdir <PATH>` | Use a specific workspace directory (default: cwd) |
| `--rebuild` | Force a rebuild of the container image |
| `--no-cache` | Build the image without the Docker/Podman layer cache |
| `--no-credential-check` | Skip scanning the workspace for credential files |
| `--dry-run` | Print podman/docker commands instead of executing them |

### Subcommands

| Command | Description |
|---|---|
| `init [--workdir PATH] [--agent ...] [--image ...]` | Create an `ai-pod.Dockerfile` in the workspace |
| `build` | Build the container image without launching |
| `attach` | Attach to a running ai-pod container session |
| `list` | List all ai-pod containers |
| `clean [--workdir PATH]` | Stop and remove the container for a workspace |
| `run <command> [args...]` | Run a command in the container instead of the default |
| `commands [list\|run\|kill\|logs]` | View/manage host commands (interactive TUI if no subcommand) |
| `services [list\|logs\|stop]` | View/manage service containers started by agents (interactive TUI if no subcommand) |
| `allowed [list\|add\|remove]` | Manage the always-allowed command whitelist (interactive TUI if no subcommand) |
| `mask <dir> [--workdir PATH]` | Shadow-mount `/app/<dir>` with an isolated per-workspace volume |
| `unmask <dir> [--workdir PATH]` | Stop masking `<dir>` and delete its shadow volume |
| `serve` | Start the shared MCP server manually (normally auto-started) |
| `update` | Fetch the latest install script and run it to upgrade |

### Run a specific command in the container

```sh
ai-pod run claude resume   # resume the last Claude session
ai-pod run bash            # open a bash shell in the container
```

### IDE integration via ACP

`ai-pod run` forwards stdio transparently between the parent process and the in-container command. When stdin is not a terminal — i.e. an IDE is piping JSON-RPC over `ai-pod`'s stdio — ai-pod drops the pseudo-TTY allocation and keeps status output on stderr, so the byte stream coming out of the container is exactly what the IDE sees. That makes any agent that speaks the [Agent Client Protocol](https://agentclientprotocol.com/) usable from inside the container.

Run your workspace through `ai-pod` once first, so the credential triage and home volume are set up. Then point your IDE at `ai-pod run …` with the in-container ACP binary as the command. For Claude Code:

```jsonc
// Zed: ~/.config/zed/settings.json
{
  "agent_servers": {
    "ai-pod (claude)": {
      "command": "ai-pod",
      "args": [
        "--no-credential-check",
        "--workdir", "/absolute/path/to/workspace",
        "run", "claude-code-acp"
      ]
    }
  }
}
```

For OpenCode, use whichever ACP entry point it exposes (e.g. `ai-pod run opencode acp`). Anything you install into your `ai-pod.Dockerfile` is on `$PATH` inside the container, so `npm i -g @zed-industries/claude-code-acp` in the Dockerfile is enough to make the example above work.

Notes:
- Pass `--no-credential-check` (or run `ai-pod` interactively first to triage the workspace) — the credential dialog can't run without a TTY, and ai-pod will refuse to start if anything is pending.
- `--workdir` is required when the IDE launches `ai-pod` from a directory other than the workspace root.

### Masking host directories

Some directories — `node_modules`, `target`, `.venv`, `dist` — contain
artifacts the container produces and the host can't (or shouldn't) reuse.
Mask them so the container gets its own per-workspace storage instead of
overlaying the host's:

```sh
ai-pod mask node_modules    # next launch mounts an isolated volume at /app/node_modules
ai-pod unmask node_modules  # stop masking and delete the volume
```

The shadow volume is named `ai-pod-<workspace-hash>-mask-<dir>` and is
removed automatically by `ai-pod clean`. Only top-level directory names are
accepted (no slashes, no hidden dirs). Changes apply to the next container
launch; a warning is printed if a container is currently running.

---

## Configuration

Your host `~/.claude/CLAUDE.md` and `~/.claude/settings.json` are merged with container defaults at launch time, and your `~/.claude.json` is copied in on first init, so your personal Claude preferences carry over automatically.

The MCP server entry for ai-pod is written into `~/.claude.json` (`mcpServers.ai-pod`) and injected into OpenCode via the `OPENCODE_CONFIG_CONTENT` env var, both with the per-session credentials baked in literally — no env-var interpolation, so `claude doctor` stays clean.

---

## Per-workspace Dockerfiles

Each workspace can have its own `ai-pod.Dockerfile` that customizes the container image — installing extra runtimes, tools, or MCP servers.

To create one in the current directory:

```sh
ai-pod init
```

This writes an `ai-pod.Dockerfile` to the workspace root based on the default image. Edit it to add anything your project needs (e.g. Node, Python, Playwright, project-specific MCP servers). When `ai-pod` launches, it automatically uses `ai-pod.Dockerfile` if present, otherwise falls back to the global default.

The default image is based on Ubuntu. The Dockerfile downloads the agent (Claude Code or OpenCode) via `curl http://${HOST_GATEWAY}:7822/install/{agent}.sh` — the shared host server vends per-agent install scripts. The generated Dockerfile includes commented-out examples for common additions like Playwright and MCP servers.

---

## Host interaction

The in-container agent talks to the host through an **MCP server** running on the shared ai-pod host server (`http://host.containers.internal:7822/mcp`, or `host.docker.internal` on Docker). No CLI binary is shipped into the container — host interaction happens entirely through MCP tools, taught to the agent via the auto-generated ai-pod skill.

### MCP tools

| Tool | What it does |
|---|---|
| `run_command` | Run a shell command on the host. Waits up to 5 s; returns inline result if finished, otherwise returns a `command_id` to poll. |
| `command_status` | Check the status of a previously started command. Returns running/finished/killed plus the last 10 lines of stdout/stderr. |
| `stop_command` | Stop a running command (SIGTERM, then SIGKILL after 5 s). |
| `list_commands` | List commands for this session (or workspace-wide with `scope=workspace`). |
| `notify_user` | Send a desktop notification to the host user. |
| `list_allowed_commands` | List host commands previously approved by the user for this workspace. |
| `start_service` | Start an auxiliary service container (e.g. `postgres:16`) reachable from inside the agent container. |
| `stop_service` | Stop and remove a service container started by this session. |
| `list_services` | List service containers started by this session. |
| `service_logs` | Read the tail of a service container's logs. |

### Service containers

The agent can spin up auxiliary containers (postgres, redis, …) it needs
for the task at hand by calling the `start_service` MCP tool. Each
request specifies an image, a short `name`, optional env vars, and
optional command override. The host user approves the image plus the
**sorted list of env-var KEY names** (values stay private and never
enter the on-disk allowlist); re-requesting the same image with the
same set of keys is auto-approved.

Service containers live on a per-workspace bridge network
(`ai-pod-<workspace-hash>-net`). The agent reaches a service by the
`name` it requested, on the service's standard port — e.g. asking for
`name=postgres image=postgres:16` makes it reachable from the agent
container as `postgres:5432`. No host port mapping is created.

Services are **ephemeral**. A fresh anonymous volume is allocated each
session and discarded when the session ends; the service container
itself is removed as soon as the main ai-pod container exits (or, as a
backstop, by a periodic sweep in the shared server). `ai-pod clean`
also removes the per-workspace network.

#### Inspecting services from the host

```sh
ai-pod services                          # interactive TUI
ai-pod services list                     # plain list across all sessions
ai-pod services logs <name> [--lines N]  # tail logs of a service
ai-pod services stop <name>              # stop a running service
```

The `--session <id>` flag disambiguates when the same name is in use
across concurrent sessions on the same workspace.

### Command output files

Every host command writes its stdout, stderr, and exit code to files on disk that the agent can read directly:

```
{workspace}/.ai-pod/commands/{session_id}/{command_id}/
  stdout       # full output stream
  stderr       # full output stream
  exit         # decimal exit code, or "killed"
  command      # the shell command string
```

The workspace is mounted at `/app` inside the container, so the agent reads these files with its normal `Read` tool. `ai-pod init` offers to add `.ai-pod` to your `.gitignore` automatically when the workspace is a git repo.

### Inspecting host commands from the host (TUI)

```sh
ai-pod commands              # interactive TUI: list, view tails, kill
ai-pod commands list [--all] # plain list (single session, or every session in the workspace)
ai-pod commands run <cmd>    # run a host command (same approval flow as the agent)
ai-pod commands kill <id>    # stop a running command
ai-pod commands logs <id>    # print stdout/stderr/exit for a command
```

TUI keybinds: `↑/↓` navigate, `Tab` toggle stdout/stderr, `k` kill the selected running command, `r` force refresh, `q` quit.

### Managing the whitelist

```sh
ai-pod allowed               # interactive TUI: list approved commands, delete with `d`
ai-pod allowed list
ai-pod allowed add <command>
ai-pod allowed remove <command>
```

When a host command isn't on the allowlist, the agent's request triggers an approval dialog on the host (60 s timeout). Approve once and it's persisted.

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

Claude can only run host commands you have explicitly approved via the interactive prompt. Approved commands are persisted per-workspace so you only approve each one once. The MCP server pre-rejects obviously dangerous patterns (e.g. starting with `cd /`, piping to `| head`/`| tail`) before they reach the approval dialog.

---

## Marketing website

A static marketing site lives in [`website/index.html`](website/index.html). Open it in any browser — no build step required.
