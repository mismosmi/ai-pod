# Security Vulnerability Analysis

**Date:** 2026-04-07
**Scope:** Full codebase review of ai-pod v0.7.0

---

## Critical Findings

### 1. Server Binds to All Network Interfaces (0.0.0.0)

**Severity:** Critical
**File:** `src/server/mod.rs:155`

```rust
let addr = SocketAddr::from(([0, 0, 0, 0], port));
```

The MCP server binds to `0.0.0.0:7822`, making it reachable from **any machine on the network**, not just localhost. Since this server exposes a `/run_command` endpoint that executes arbitrary shell commands on the host, any attacker on the same network who can guess or obtain a project API key can execute commands remotely.

**Impact:** Remote code execution from any host on the local network.

**Fix:** Bind to `127.0.0.1` instead:
```rust
let addr = SocketAddr::from(([127, 0, 0, 1], port));
```

Note: Podman containers reach the host via `host.containers.internal` which resolves to the host's gateway IP and routes to `127.0.0.1` services, so binding to localhost should still work for the intended container-to-host communication. If not, bind to the container bridge interface specifically rather than all interfaces.

---

### 2. macOS AppleScript Injection in Command Approval Dialog

**Severity:** High
**File:** `src/server/commands.rs:59-63`

```rust
let script = format!(
    r#"display dialog "Run command:\n{}" buttons {{"Allow Once","Always Allow","Deny"}} default button "Deny" with title "ai-pod: {}""#,
    command.replace('"', "\\\""),
    project_name.replace('"', "\\\""),
);
```

The command string is interpolated into an AppleScript template with only double-quote escaping. AppleScript allows backslash sequences and other metacharacters. A malicious command containing `\" & do shell script \"malicious_command` (or similar sequences) could break out of the string context and execute arbitrary AppleScript, including `do shell script` for code execution — **before** the user even approves the command.

**Impact:** Code execution on macOS hosts through crafted command strings, bypassing the approval dialog entirely.

**Fix:** Sanitize or escape all AppleScript special characters, or pass the command string via a safer mechanism (e.g., environment variable read by osascript, or stdin).

---

## High Severity Findings

### 3. Non-Constant-Time API Key Comparison

**Severity:** Medium-High
**Files:** `src/server/rest.rs:97`, `src/server/daemons.rs:146`

```rust
Some(info) if info.api_key != provided_key => {
    Err((StatusCode::UNAUTHORIZED, "Invalid API key"))
}
```

API keys are compared using standard string inequality (`!=`), which is vulnerable to timing attacks. An attacker could theoretically determine the API key character-by-character by measuring response times.

**Practical impact:** Low when the server is bound to localhost (latency noise dominates), but **high when combined with Finding #1** (network-accessible server where timing measurements are more feasible with statistical methods).

**Fix:** Use constant-time comparison. Add the `subtle` crate or implement a simple constant-time equality check:
```rust
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() { return false; }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

### 4. State Files Written Without Explicit Permissions

**Severity:** Medium-High
**File:** `src/server/lifecycle.rs:38-44`

```rust
pub fn save(&self, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(self)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json).context("Failed to write state file")?;
    std::fs::rename(&tmp, path).context("Failed to rename state file")?;
    Ok(())
}
```

Project state files (`~/.ai-pod/{hash}.json`) contain API keys in plaintext and are written with the process's default umask. On systems with a permissive umask (e.g., `0022`), these files are world-readable.

Similarly, `server.json` (line 113) is written without explicit permissions.

**Impact:** Local privilege escalation — any user on the system can read API keys and use them to execute commands via the server.

**Fix:** Set file permissions to `0o600` explicitly:
```rust
use std::os::unix::fs::PermissionsExt;
std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
```

### 5. Pipe-to-Shell Installation Pattern

**Severity:** Medium-High
**File:** `src/container.rs:24`

```rust
const SETUP_SCRIPT: &str = r#"#!/bin/sh
set -e
export PATH="$HOME/.local/bin:$PATH"
curl -fsSL https://claude.ai/install.sh | bash
"#;
```

The Claude Code installation inside containers uses `curl | bash`, which is vulnerable to:
- **Man-in-the-middle attacks** if TLS is compromised
- **Partial download execution** — if the connection drops mid-download, a truncated script may execute with unintended behavior
- **Time-of-check-time-of-use** — the script content could change between when you audit it and when it runs

**Impact:** Arbitrary code execution inside the container if the download is tampered with.

**Fix:** Download to a file first, verify a checksum, then execute. Or pin a specific version.

---

## Medium Severity Findings

### 6. Container Runs Claude Code with `bypassPermissions`

**Severity:** Medium
**File:** `src/container.rs:165-168`

```rust
perms_obj.insert(
    "defaultMode".to_string(),
    serde_json::Value::String("bypassPermissions".to_string()),
);
```

The generated `settings.json` sets Claude Code to `bypassPermissions` mode inside the container. While the container provides isolation, this means Claude Code can execute **any** command inside the container without user approval, including network operations, file modifications to the mounted workspace (`/app:Z` is read-write), and calls to `host-tools` (which then go through approval on the host side).

**Impact:** A compromised or misbehaving Claude session can modify all workspace files without permission prompts. The workspace is mounted read-write with SELinux relabeling (`:Z`), so changes affect the host filesystem directly.

### 7. Unpinned Docker Base Images

**Severity:** Medium
**Files:** `claude.Dockerfile:1`, `ai-pod.Dockerfile:1`

```dockerfile
FROM ubuntu:latest
FROM rust:latest
```

Using `:latest` tags means builds are non-reproducible and vulnerable to supply chain attacks if the upstream image is compromised.

**Fix:** Pin to specific digests:
```dockerfile
FROM ubuntu:24.04@sha256:<digest>
```

### 8. No Rate Limiting on Server Endpoints

**Severity:** Medium
**File:** `src/server/mod.rs:140-153`

No rate limiting is applied to any endpoint, including `/run_command`. An attacker (especially when combined with Finding #1) could flood the approval notification system or attempt brute-force API key guessing.

**Fix:** Add a rate-limiting middleware (e.g., `tower::limit::RateLimitLayer`) to sensitive endpoints.

### 9. Server Log May Contain Sensitive Information

**Severity:** Medium
**File:** `src/server/lifecycle.rs:98-100`

```rust
let log_path = config.config_dir.join("server.log");
let log = std::fs::File::create(&log_path).context("Failed to create server log file")?;
```

The server log file is created without explicit permissions and may contain request details, command strings, or error messages that include sensitive information. Same umask concern as Finding #4.

---

## Low Severity Findings

### 10. `host-tools` Binary Downloaded Over HTTPS Without Integrity Verification

**Severity:** Low-Medium
**File:** `src/container.rs:189-207`

The `host-tools` binary is downloaded from GitHub releases but its integrity is not verified (no checksum or signature check). If the GitHub release is compromised or a MITM attack succeeds against HTTPS, a malicious binary could be placed in the container.

### 11. `install.sh` Does Not Verify Download Integrity

**Severity:** Low-Medium
**File:** `install.sh:52-58`

Same issue as above — the install script downloads the binary without checksum verification.

### 12. Incomplete Pipe Rejection in Command Validation

**Severity:** Low
**File:** `src/server/commands.rs:150-157`

```rust
pub fn ends_with_pipe_to_head_or_tail(cmd: &str) -> bool {
    if let Some(pipe_pos) = cmd.trim_end().rfind('|') {
        let after = cmd[pipe_pos + 1..].trim_start();
        let word = &after[..after.find(|c: char| c.is_whitespace()).unwrap_or(after.len())];
        return word == "head" || word == "tail";
    }
    false
}
```

This only checks the **last** pipe segment. Commands like `cmd | head -5 | cat` would bypass the check. Additionally, shell features like `$(head)` or backtick substitution are not considered.

### 13. Daemon Command Execution Without Per-Command Approval

**Severity:** Low
**File:** `src/server/daemons.rs` (daemon start handler)

Daemon commands are started with `sh -c` and appear to go through authentication but may not go through the same command approval flow as `/run_command`. If the daemon start handler skips the approval gate, it would be a privilege escalation within the command execution model.

### 14. PID Reuse Race Condition

**Severity:** Low
**File:** `src/server/lifecycle.rs:73-74`

```rust
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
```

Between checking if a PID is alive and acting on that result, the PID could be recycled by the OS to a different process. This is a minor TOCTOU issue.

---

## Positive Security Observations

- **Per-project API key isolation** using UUID v4 (cryptographically random)
- **Credential scanning** with comprehensive pattern matching before mounting into containers
- **Atomic file writes** using temp file + rename pattern
- **Safe argument passing** to podman commands (using `.args()` arrays, not shell interpolation)
- **Process group management** for clean daemon cleanup
- **rustls-tls** instead of OpenSSL (reduced attack surface)
- **User approval gate** for host command execution (when the server is only on localhost)
- **Container isolation** with user namespaces (`--userns=keep-id`)
- **Graceful error handling** throughout with `anyhow::Result`

---

## Recommended Priority Order

| Priority | Finding | Effort |
|----------|---------|--------|
| P0 | #1 — Bind to 127.0.0.1 | Trivial (1 line) |
| P0 | #2 — AppleScript injection | Small |
| P1 | #4 — File permissions on state files | Small |
| P1 | #3 — Constant-time key comparison | Small |
| P2 | #7 — Pin Docker image versions | Small |
| P2 | #5 — Verify install script integrity | Medium |
| P2 | #6 — Document bypassPermissions risk | Small |
| P3 | #8 — Rate limiting | Medium |
| P3 | #10/#11 — Binary integrity verification | Medium |
| P3 | #12 — Improve pipe rejection | Small |
