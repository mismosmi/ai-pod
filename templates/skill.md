---
name: ai-pod
description: This skill should be used when the user asks to run a command on the host machine, open an application on the host, send a desktop notification to the user, list previously approved host commands, or manage long-running background processes (daemons) on the host. Provides the host-tools binary at /home/ai-pod/.local/bin/host-tools.
version: 0.1.0
---
# host-tools — Host Interaction

`/home/ai-pod/.local/bin/host-tools` interacts with the host machine from inside this container.

## run-command

Run a shell command on the host. The host user is prompted to approve commands not previously allowed. Output streams back in real time.

    host-tools run-command <shell command and args>

Examples:
- `host-tools run-command pnpm tsc`
- `host-tools run-command podman compose up -d`
- `host-tools run-command cargo build`

DO NOT start commands with `cd /some/path && ...`. The working directory is already set to the project workspace. Commands starting with `cd /` are rejected by the server.

YOU MUST NOT TRIM OUTPUT ON THE HOST.
do not use `host-tools run-command 'command | head -n 10'` to trim output.
ALWAYS use head or tail in the container instead: `host-tools run-command 'command' | head -n 10`
Keep the commands you run on the host as simple as possible.
host-tools run-command forwards all output (stdout and stderr).

List previously approved commands:

    host-tools run-command --list

If a command is in the list, always run it on the host.
If a command is not in the list, prefer to run it inside the container.

For long-running commands prefer the daemon-subcommands.

## notify-user

Send a desktop notification to the host user. The notification title is set automatically to the project name.

    host-tools notify-user "<message>"

Example: `host-tools notify-user "Build finished successfully"`

A Stop hook already calls this automatically when the session ends.

## daemon

Use this if you need to run a dev server or need to run long-running tests. Pipe `host-tools daemon output <daemon-id>` into `grep`, `head`, `tail` etc to evaluate output.

Manage long-running background processes on the host.

    host-tools daemon start <shell command>   # returns daemon ID
    host-tools daemon list                    # show all daemons for this project
    host-tools daemon output <daemon-id>      # print log and exit
    host-tools daemon status <daemon-id>      # running/finished, exit code
    host-tools daemon stop <daemon-id>
    host-tools daemon stop-all

The same rules apply to daemon commands as to regular run-commands.
