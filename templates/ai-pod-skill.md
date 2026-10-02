---
name: ai-pod
description: How to work inside an ai-pod container — running commands on the host and adjusting the container environment.
---

# Working inside an ai-pod

You are running in an ai-pod: a container dedicated to this workspace. Anything
outside the container (the user's machine, their browser, other repos, the
container runtime itself) is reachable only through the `ai-pod` MCP server.

## Where you are

- **The container starts in the workspace directory.** Workspaces outside Linux
  system directories use the same path as on the host (including `/home` and
  `/Users`); workspaces in system directories are mounted at `/app`.
  Files you write there land on the user's real filesystem. Linked worktrees
  outside system directories also expose the main checkout and share its persistent home volume.
- **`$HOME` is a persistent volume.** Agent config and login survive restarts.
  It is _not_ the user's home directory.
- **Everything else in the container is ephemeral.** Packages you install at
  runtime disappear when the session ends. To make a tool permanent, edit
  `{{DOCKERFILE}}` (see _Changing this container_ below).
- **The host is `{{HOST_GATEWAY}}`, not `localhost`.** A dev server the user
  runs on their machine is at `{{HOST_GATEWAY}}:<port>` from in here.

## Running commands on the host

Use the `run_command` MCP tool. Each distinct command string needs the user's
approval through a desktop dialog; `list_allowed_commands` shows what they have
already allowed for this workspace.

Rules, in order of how often they matter:

1. **Prefer the container.** Only go to the host for things that genuinely
   cannot work in here: host-only tools, the user's browser or GUI, other
   checkouts, `podman`/`docker`, services bound to the host.
2. **Keep the command simple.** One command with plain arguments. Do not build
   pipelines, do not redirect, do not chain with `&&` or `;`, and do not wrap
   things in `bash -c "..."` to shape the output.
3. **Never trim output on the host.** stdout and stderr are captured to files
   you can read in full (see below), so `| head`, `| tail`, `| grep` and `> file`
   buy you nothing — `| head` and `| tail` are rejected outright.
   Run the plain command and read as much of the output file as you need.
4. **Don't `cd` to an absolute path first.** Host commands already start in the
   workspace root, and a leading `cd /…` is rejected. A relative `cd sub && …`
   is fine when a tool insists on a subdirectory.
5. **Repeat command strings verbatim.** The user's allowlist matches on the
   exact string, so re-running the identical command is silent while an added
   flag or pipe pops a fresh approval dialog.

### Reading the output

`run_command` waits up to 5 seconds. If the command is still running you get a
`command_id` back and the output keeps streaming into:

```
./.ai-pod/commands/{session_id}/{command_id}/stdout
./.ai-pod/commands/{session_id}/{command_id}/stderr
./.ai-pod/commands/{session_id}/{command_id}/exit
```

These are files in _this_ container (relative to the workspace directory), so
read them with your normal file Read tool — not with a host command. Re-read
`stdout` to follow progress and `exit` to see whether it is done: it contains
the decimal exit code, or `killed`. `command_status` is there for a quick
status plus a 10-line tail; it is not a polling loop, the files are.

### Stopping a command

Use `stop_command` with the `command_id`. ai-pod runs every host command in its
own process group and sends SIGTERM, then SIGKILL after 5 seconds, to the whole
group. `list_commands` shows what this session started, with ids.

**Do not run `kill`, `pkill`, `killall`, or `xargs kill` on the host.** You
would be guessing at pids on the user's machine, and you can just as easily hit
their editor, their dev server, or this session. If something is running on the
host that ai-pod did not start, tell the user about it instead of killing it.

## Service containers

Need a database, cache, or broker? Do not install it into this container — call
`start_service` with an image (`postgres:16`), a short `name`, and any env vars.
The service joins this workspace's network and is reachable from here by that
name on the image's standard port, e.g. `postgres:5432`. `list_services`,
`service_logs`, and `stop_service` manage them. Services are ephemeral: their
data is discarded when the session ends.

## Getting the user's attention

`notify_user` sends a desktop notification. Use it when a long job finished or
you are blocked on a question and the user has walked away.

## Changing this container

The image is built from `./{{DOCKERFILE}}`. Edit that file to add tools,
runtimes, or system packages permanently.

Keep these intact:

- `ARG HOST_GATEWAY` / `ARG AI_POD_VERSION` and the
  `RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/<agent>.sh" | bash`
  line — that is what installs the agent you are running as.
- `WORKDIR /app`, the `ai-pod` user creation, `USER ai-pod`, and the final
  `CMD`.

Put root-level installs (`apt-get install …`, toolchains) _before_ the
`USER ai-pod` line and user-level installs after it.

Then verify with the `rebuild_image` MCP tool:

```json
{ "test_command": "node --version && npx playwright --version" }
```

- It rebuilds the image from the current `{{DOCKERFILE}}` and, if the build
  succeeds, runs `test_command` in a throwaway container of the fresh image.
  The container is removed immediately; nothing in it persists, and the
  workspace is not mounted, so test for tools rather than for project builds.
- Build log and test output land in the same command output files as
  `run_command`, with a `command_id` to follow if it takes more than 5 seconds.
  A non-zero `exit` means either the build or the test failed — read `stdout`
  and `stderr` to find out which.
- Write a `test_command` that actually proves what you added is installed and
  on `PATH`.
- Do **not** run `podman build` or `docker build` through `run_command`;
  `rebuild_image` is approved separately and knows the right image name, build
  args, and context.
- **The rebuild does not change the container you are in right now.** The new
  image is picked up the next time the user starts ai-pod. When your change
  matters for the current task, say so and ask the user to restart the pod.
