# ai-pod

[Read the docs](https://ai-pod.apps.farbenmeer.de)

**Claude Code, OpenCode & Codex inside isolated containers — safe, persistent, and project-aware.**

ai-pod manages per-workspace containers that run Claude Code, OpenCode, or OpenAI Codex. It works with **Podman** (preferred) or **Docker** — whichever is available on your system. Each workspace gets a dedicated container, a shared background server bridges host interaction via MCP, and your personal agent settings follow you everywhere.

---

## Features

- **Workspace isolation** — each directory gets its own container, named by a hash of its path; projects can't interfere with each other
- **Persistent agent state** — a named volume preserves `~/.claude`, `~/.config/opencode`, and `~/.codex` (login, memory, settings) across container restarts
- **Credential scanning** — scans the workspace for secrets before mounting it; prompts you to review or abort
- **Custom Dockerfiles per project** — drop an `ai-pod.Dockerfile` in any project to install extra runtimes, tools, or MCP servers
- **AI-driven skill file** — an `ai-pod` skill is written into every container so the agent can look up how the pod works (host commands, services, image rebuilds) on demand instead of carrying it in context
- **Self-adjusting environment** — the agent can edit `ai-pod.Dockerfile` and verify it with the `rebuild_image` MCP tool, which rebuilds the image and smoke-tests it in a throwaway container
- **Host command execution via MCP** — the in-container agent talks to the shared host server over MCP (`http://host.containers.internal:7822/mcp`); every host command requires your explicit approval with a persistent allowlist
- **File-based command output** — every command writes stdout/stderr/exit to `{workspace}/.ai-pod/commands/{session_id}/{command_id}/` so the agent reads long-running output directly
- **Interactive TUIs** — `ai-pod commands` to inspect/kill running host commands, `ai-pod allowed` to manage the whitelist
- **Desktop notifications** — Stop hooks notify you on Claude session end, an OpenCode plugin sends notifications when `session.idle` fires, and Codex's `notify` program fires on turn completion
- **Rate-limit approval** — when a client exhausts the host API's request budget, a desktop notification offers a one-time counter reset. Choose **Wait for cooldown** (or dismiss it) to keep throttling; clients receive HTTP 429 with `Retry-After` until requests can resume. Repeated requests do not duplicate the prompt.
- **Transparent host networking** — containers reach host services at `host.containers.internal` (Podman) or `host.docker.internal` (Docker); no manual port mapping needed
- **Browser control on the host** — `--playwright` starts Playwright MCP outside the container, so the agent drives a real, visible browser on your desktop
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
| `-p, --publish <PORT>` | Publish a container port; repeatable and passed directly to Podman/Docker (e.g. `ai-pod -p 8080:80` or `ai-pod -p 8080:80 run bash`) |
| `--dry-run` | Print podman/docker commands instead of executing them |
| `--playwright` | Start Playwright MCP on the host and wire it into the agent (see [Browser control](#browser-control-with-playwright)) |

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
| `mask add <dir> [--workdir PATH]` | Shadow-mount `/app/<dir>` with an isolated per-workspace volume |
| `mask list [--workdir PATH]` | List masked directories for the workspace |
| `mask remove <dir> [--workdir PATH]` | Stop masking `<dir>` and delete its shadow volume |
| `egress [status\|proxy\|image\|off] [--workdir PATH]` | Filter the agent's network traffic through an HTTP proxy |
| `serve` | Start the shared MCP server manually (normally auto-started) |
| `update` | Fetch the latest install script and run it to upgrade |

After an update, starting a client against an older shared server shows the
running sessions across Docker and Podman and asks whether to restart the server.
Answering yes waits for the old server to quit, then starts the installed version.
Containers remain running, but server access is briefly interrupted and tracking
of running host commands is lost. Answering no leaves the server running and
cancels startup. Noninteractive clients require an interactive launch to approve
the restart. Servers from before this restart protocol must exit once (after all
sessions finish) before this flow is available.

Workspaces outside Linux system directories are mounted at their host path, and the container starts
in that directory. For Git worktrees, ai-pod also mounts the main checkout; a
worktree nested inside it (for example in `.ai-pod/worktrees`) needs only the
main checkout mount. The main checkout and its worktrees share a persistent
home volume. Workspaces under Linux system directories (such as `/etc`, `/usr`, `/var`,
`/tmp`, `/opt`, and `/root`) retain the `/app` mount and separate
home volumes. `clean` on a checkout or worktree targets that shared home volume;
the runtime will refuse to remove it while another container is using it.

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

### Browser control with Playwright

```sh
ai-pod --playwright
```

Starts `npx @playwright/mcp@latest --port 8931 --host 0.0.0.0` **on the host** and
adds a `playwright` MCP server to whichever agent the container runs (Claude,
OpenCode or Codex). The browser therefore runs outside the pod, on your desktop, in a headed window
you can watch. Playwright keeps its profile between runs, so a site you log into
once stays logged in for later sessions — the agent can click through an app
behind your authentication instead of a fresh, empty container browser.

Two details make the container -> host hop work, and ai-pod handles both:

- the MCP url uses the runtime's gateway name (`host.containers.internal` for
  Podman, `host.docker.internal` for Docker) rather than `localhost`;
- Playwright MCP refuses requests whose `Host` header is not in its allow-list,
  so the MCP entry sends a spoofed `Host: localhost:8931` (and the server is
  additionally started with both gateway names in `--allowed-hosts`).

Requires Node.js 18+ on the host (`npx`). The first launch downloads the package
and can take a while; output goes to `~/.ai-pod/playwright.log`.

The server is stopped again when the session that started it exits; a server
that was already running (another session, or one you started by hand) is
reused and left alone. Launching **without** `--playwright` removes the
`playwright` MCP entry again, so the agent only ever sees it when you asked for
it. An entry you added yourself pointing somewhere else is never touched.

> **Note:** the Playwright MCP server binds all interfaces and has no
> authentication — while it runs, anyone who can reach port 8931 on your machine
> can drive your browser. Only use `--playwright` on a trusted network.

### Masking host directories

Some directories — `node_modules`, `target`, `.venv`, `dist` — contain
artifacts the container produces and the host can't (or shouldn't) reuse.
Mask them so the container gets its own per-workspace storage instead of
overlaying the host's:

```sh
ai-pod mask add node_modules # next launch mounts an isolated volume at /app/node_modules
ai-pod mask remove node_modules # stop masking and delete the volume
```

The shadow volume is named `ai-pod-<workspace-hash>-mask-<dir>` and is
removed automatically by `ai-pod clean`. Only top-level directory names are
accepted (no slashes, no hidden dirs). Changes apply to the next container
launch; a warning is printed if a container is currently running.

### Filtering network access

By default the agent has unrestricted network access. `ai-pod egress` routes
all of a workspace's traffic through an HTTP proxy of your choice, which
decides what is allowed (for example squid with an allow-list):

```sh
ai-pod egress proxy localhost:3128   # use a proxy you already run (localhost = the host machine)
ai-pod egress image docker.io/ubuntu/squid \
  -v ~/squid.conf:/etc/squid/squid.conf:ro   # or let ai-pod start one per session
ai-pod egress                        # show the current setting
ai-pod egress off                    # back to unrestricted access
```

With a filter configured, the container and any service containers it starts
join an internal network with no route outside. A small per-session gateway
container (`alpine/socat`) is the only way out: it forwards the ai-pod server,
Playwright (if enabled) and the proxy port. `HTTP_PROXY`/`HTTPS_PROXY` point at
it, and tools that ignore those variables can't connect at all. Without a
filter, none of this runs.

Notes:

- `--playwright` drives a browser **on the host**, which the filter does not
  cover. ai-pod warns and asks for confirmation before starting with both.
- Ports published with `-p` are not reachable while filtering is on.
- HTTP services started with `start_service` are reached through the proxy
  too, so allow them there if needed.
- Changes apply to sessions started afterwards.

---

## Configuration

Your host `~/.claude/CLAUDE.md` and `~/.claude/settings.json` are merged with container defaults at launch time, and your `~/.claude.json` is copied in on first init, so your personal Claude preferences carry over automatically.

The MCP server entry for ai-pod is written into `~/.claude.json` (`mcpServers.ai-pod`), injected into OpenCode via the `OPENCODE_CONFIG_CONTENT` env var, and merged into Codex's `~/.codex/config.toml` (`[mcp_servers.ai-pod]`, a streamable-HTTP MCP server), all with the per-session credentials baked in literally — no env-var interpolation, so `claude doctor` stays clean. Your personal Codex login (`~/.codex/auth.json`) and other `config.toml` preferences (model, provider) carry over and are preserved; ai-pod only rewrites the keys it owns.

---

## Per-workspace Dockerfiles

Each workspace can have its own `ai-pod.Dockerfile` that customizes the container image — installing extra runtimes, tools, or MCP servers.

To create one in the current directory:

```sh
ai-pod init
```

This writes an `ai-pod.Dockerfile` to the workspace root based on the default image. Edit it to add anything your project needs (e.g. Node, Python, Playwright, project-specific MCP servers). When `ai-pod` launches, it automatically uses `ai-pod.Dockerfile` if present, otherwise falls back to the global default.

The default image is based on Ubuntu. The Dockerfile downloads the agent (Claude Code, OpenCode, or Codex) via `curl http://${HOST_GATEWAY}:7822/install/{agent}.sh` — the shared host server vends per-agent install scripts. The generated Dockerfile includes commented-out examples for common additions like Playwright and MCP servers.

---

## The ai-pod skill

Every container gets an `ai-pod` skill written into the home volume, at
`~/.claude/skills/ai-pod/SKILL.md` (read by Claude Code and OpenCode) and
`~/.codex/skills/ai-pod/SKILL.md` (read by Codex). It is refreshed on every
launch, so upgrading ai-pod updates the instructions in existing volumes.

Agents load a skill on demand, so the guidance costs nothing until the agent
actually needs it. It covers:

- **Where the agent is** — workspace at `/app`, a persistent `$HOME` volume,
  everything else ephemeral, the host at `host.containers.internal` /
  `host.docker.internal`.
- **How to run host commands** — prefer the container; keep the command simple;
  never pipe, redirect, or chain just to shape output, because the full streams
  are already written to files the agent can read; don't `cd` to an absolute
  path; reuse command strings verbatim so the user's allowlist keeps matching.
- **How to stop them** — `stop_command` with the `command_id`, never `kill`,
  `pkill` or `killall` on the host: those guess at pids on the user's machine
  and can take out their editor, their dev server, or the session itself.
- **Service containers** — reach for `start_service` instead of installing a
  database into the pod.
- **How to change the image** — edit `ai-pod.Dockerfile`, what must stay intact,
  and how to verify the result with `rebuild_image`.

Bind-mounting your own skills directory over the container's (e.g.
`ai-pod mount add ~/.claude/skills`) hides the injected copy; ai-pod prints a
warning at launch when a mount does that.

---

## Host interaction

The in-container agent talks to the host through an **MCP server** running on the shared ai-pod host server (`http://host.containers.internal:7822/mcp`, or `host.docker.internal` on Docker). No CLI binary is shipped into the container — host interaction happens entirely through MCP tools, taught to the agent via the injected [ai-pod skill](#the-ai-pod-skill).

### MCP tools

| Tool | What it does |
|---|---|
| `run_command` | Run a shell command on the host. Waits up to 5 s; returns inline result if finished, otherwise returns a `command_id` to poll. |
| `command_status` | Check the status of a previously started command. Returns running/finished/killed plus the last 10 lines of stdout/stderr. |
| `stop_command` | Stop a running command (SIGTERM, then SIGKILL after 5 s). |
| `list_commands` | List commands for this session (or workspace-wide with `scope=workspace`). |
| `rebuild_image` | Rebuild the workspace image from `ai-pod.Dockerfile` and run a test command in a throwaway container of the result. |
| `notify_user` | Send a desktop notification to the host user. |
| `list_allowed_commands` | List host commands previously approved by the user for this workspace. |
| `start_service` | Start an auxiliary service container (e.g. `postgres:16`) reachable from inside the agent container. |
| `stop_service` | Stop and remove a service container started by this session. |
| `list_services` | List service containers started by this session. |
| `service_logs` | Read the tail of a service container's logs. |

### Rebuilding the image from inside the pod

The agent can adjust its own environment: it edits `/app/ai-pod.Dockerfile` and
then calls `rebuild_image` with a `test_command`.

```jsonc
{ "test_command": "node --version && npx playwright --version" }
```

The tool rebuilds the workspace image with the same build args and context the
CLI uses and, **only if the build succeeds**, runs `test_command` in a throwaway
container of the fresh image (`--rm`, no workspace mount) so the agent can prove
the tools it added are installed and on `PATH`. Build log and test output stream
to the usual command output files, so a long build is polled by re-reading
`stdout` rather than by blocking.

Rebuilds are approved like everything else, with their own per-workspace
allowlist entry (`rebuild image <image> from ai-pod.Dockerfile`) that is
independent of the test command — so approving once lets the agent iterate on
its Dockerfile, and each rebuild is still just a container build on your machine.

The **running container is not affected**: the new image is picked up the next
time you start ai-pod.

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

The agent reads these files with its normal `Read` tool relative to the workspace directory. `ai-pod init` offers to add `.ai-pod` to your `.gitignore` automatically when the workspace is a git repo.

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

Detection is regex-based, so any `.env` variant is matched — `.env.local`, `.env.dev`, `.env.whatever`. Templates like `.env.example` match too; pick "keep in workspace, suppress future warnings" once and they won't be reported again for that project.

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

Service starts and image rebuilds are approved the same way, in their own
per-workspace buckets: a service is keyed by image plus the sorted list of
env-var KEY names, a rebuild by the image name. Approving one never implies the
others — an always-allowed `make build` does not let the agent rebuild its
image, and an always-allowed rebuild does not let it run arbitrary host
commands.

---

## Marketing website

A static marketing site lives in [`website/index.html`](website/index.html). Open it in any browser — no build step required.
