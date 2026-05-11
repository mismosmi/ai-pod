---
name: ai-pod
description: Use when the user asks to run a host command, send a desktop notification, or inspect a previously started host command. Tools are exposed via the ai-pod MCP server (no in-container CLI).
version: 0.2.0
---
# Container Environment

You are running inside a {{ display_name }} container. To reach services on the
host machine, use `{{ host_gateway }}` instead of `localhost`.

For example: `curl http://{{ host_gateway }}:3000`

Working directory: /app

# Host interaction (MCP tools)

The `ai-pod` MCP server exposes the following tools. Use them via your MCP
tool-calling interface — there is no `host-tools` binary.

## run_command

Run a shell command on the host. The host user is asked to approve commands
not previously allowed.

- The call returns within ~5 seconds.
- If the command finished in time, you get `status: "finished"`, the exit
  code, and the last 10 lines of stdout/stderr.
- If it is still running, you get `status: "running"` and a `command_id`.

All output is always written to:

    /app/.ai-pod/commands/{session_id}/{command_id}/
        stdout
        stderr
        exit       (written when the command finishes; "killed" if stopped)
        command    (the original shell string)

You can read those files directly with the Read tool — that is the canonical
way to inspect long-running output.

DO NOT start commands with `cd /...`. The working directory is already set to
the workspace.

DO NOT pipe to `| head` or `| tail` on the host. Read the stdout file inside
the container and trim with `head`/`tail` there.

## command_status

Given a `command_id`, returns running / finished / killed plus the latest
stdout/stderr tails. Equivalent to checking whether the `exit` file exists.

## stop_command

Sends SIGTERM (then SIGKILL after 5s) to a running command.

## list_commands

Lists commands for this session by default; pass `scope: "workspace"` to see
all sessions for the workspace.

## notify_user

Sends a desktop notification to the host user. A Stop hook already calls this
automatically when the session ends.

## list_allowed_commands

Lists the host commands the user has previously approved for this workspace.
If a command is in this list, prefer running it on the host. Otherwise prefer
running inside the container.
