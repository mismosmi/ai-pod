//! Host-side `ai-pod mount` subcommand: add, remove, list bind-mounts that
//! are applied to every ai-pod container launch.

use anyhow::Result;
use colored::Colorize;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::config::{AppConfig, GlobalConfig, MountSpec};

const CONTAINER_HOME: &str = "/home/ai-pod";

/// Container path prefixes that would break the container if shadowed by a
/// user mount.
const RESERVED_CONTAINER_PREFIXES: &[&str] =
    &["/proc", "/sys", "/dev", "/etc", "/tmp", "/run", "/var/run"];

/// Exact container paths that ai-pod itself seeds into the home volume
/// (see `seed_home_volume` in container.rs). Mounting a host path on top of
/// any of these would silently replace ai-pod's Stop hook / MCP wiring /
/// opencode plugin and produce a container that looks fine but no longer
/// communicates with the host. Sub-paths are allowed (e.g.
/// `/home/ai-pod/.claude/skills` is the advertised use case).
const SEEDED_CONTAINER_TARGETS: &[&str] = &[
    "/home/ai-pod/.claude",
    "/home/ai-pod/.config/opencode/plugins",
];

/// Parse a user-provided `host[:container]` spec. The host portion is
/// tilde-expanded against `home_dir`; both halves are normalized (trailing
/// slashes trimmed) before validation so dedup and the seeded-path /
/// reserved-prefix checks can't be bypassed by writing `~/x/` vs `~/x` or
/// `/home/ai-pod/` vs `/home/ai-pod`.
pub(crate) fn parse_spec(s: &str, writable: bool, home_dir: &Path) -> Result<MountSpec> {
    let (host_raw, container_raw) = match s.split_once(':') {
        Some((h, c)) => (h, Some(c)),
        None => (s, None),
    };
    let host = normalize_host(host_raw, home_dir);
    let container = container_raw.map(normalize_container);
    let spec = MountSpec {
        host,
        container,
        writable,
    };
    validate_spec(&spec, home_dir)?;
    Ok(spec)
}

/// Expand a leading `~` / `~/` against `home_dir` and strip any trailing
/// slashes. Used as the single normalization point for both `mount add` and
/// `mount remove` so they look at exactly the same string.
pub(crate) fn normalize_host(s: &str, home_dir: &Path) -> String {
    let expanded = if s == "~" {
        home_dir.display().to_string()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir.join(rest).display().to_string()
    } else {
        s.to_string()
    };
    trim_trailing_slashes(&expanded)
}

pub(crate) fn normalize_container(s: &str) -> String {
    trim_trailing_slashes(s)
}

fn trim_trailing_slashes(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        // Preserve "/" so the host-root check in `validate_host_path` can
        // reject it explicitly rather than producing a misleading
        // "must not be empty" error.
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn validate_host_path(p: &str) -> Result<()> {
    if p.is_empty() {
        anyhow::bail!("Host path must not be empty");
    }
    if p.contains('\0') {
        anyhow::bail!("Host path must not contain null bytes");
    }
    if p.contains(':') {
        // The `-v` arg uses `:` to separate host:container:opts. A host path
        // with a `:` either smuggles in mount options (e.g. host
        // `/x:rw,suid`) or is silently truncated to the first colon, both
        // of which surprise the user.
        anyhow::bail!("Host path must not contain ':' (collides with -v separator)");
    }
    if p == "/" {
        anyhow::bail!(
            "Host path '/' (filesystem root) is not allowed; mounting it would expose \
             the entire host filesystem to the container"
        );
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        anyhow::bail!(
            "Host path must be absolute (got {}). Use ~/path or /absolute/path.",
            p
        );
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("Host path must not contain '..' segments");
    }
    Ok(())
}

pub(crate) fn validate_container_path(p: &str) -> Result<()> {
    if p.is_empty() {
        anyhow::bail!("Container path must not be empty");
    }
    if p.contains('\0') {
        anyhow::bail!("Container path must not contain null bytes");
    }
    if p.contains(':') {
        anyhow::bail!("Container path must not contain ':' (collides with -v separator)");
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        anyhow::bail!("Container path must be absolute (got {})", p);
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("Container path must not contain '..' segments");
    }
    if p == "/" {
        anyhow::bail!("Container path '/' is not a valid mount target");
    }
    if p == CONTAINER_HOME {
        anyhow::bail!(
            "Container target {} would shadow the entire home volume; pick a sub-path",
            CONTAINER_HOME
        );
    }
    if p == "/app" || p.starts_with("/app/") {
        anyhow::bail!("Container target must not be under /app (workspace bind)");
    }
    for reserved in RESERVED_CONTAINER_PREFIXES {
        if p == *reserved || p.starts_with(&format!("{}/", reserved)) {
            anyhow::bail!("Container target {} is reserved", p);
        }
    }
    for seeded in SEEDED_CONTAINER_TARGETS {
        if p == *seeded {
            anyhow::bail!(
                "Container target {} is seeded by ai-pod and would silently shadow \
                 the in-container settings. Mount a sub-path instead (e.g. {}/skills).",
                p,
                p
            );
        }
    }
    Ok(())
}

/// Re-run all validators against a stored `MountSpec`, returning the resolved
/// container target on success. Called both at `mount add` time (via
/// [`parse_spec`]) and at every container launch by
/// `container::build_mount_args` so that a hand-edited `~/.ai-pod/config.json`
/// can't bypass the security and footgun checks.
pub(crate) fn validate_spec(spec: &MountSpec, home_dir: &Path) -> Result<String> {
    validate_host_path(&spec.host)?;
    if let Some(c) = &spec.container {
        validate_container_path(c)?;
    } else {
        // Auto-mode: host must be under $HOME so the resolver can mirror it.
        // We check here (in addition to inside `resolve_container_target`)
        // so the error is actionable at `mount add` time.
        let host = Path::new(&spec.host);
        if host.strip_prefix(home_dir).is_err() {
            anyhow::bail!(
                "Host path {} is outside $HOME. Supply an explicit container path: \
                 ai-pod mount add {}:<container-path>",
                spec.host,
                spec.host
            );
        }
    }
    let target = crate::container::resolve_container_target(spec, home_dir)?;
    // Re-validate the *resolved* target so the seeded-path / reserved /
    // /home/ai-pod-root checks apply to auto-mode mounts too — e.g.
    // `mount add ~/.claude` resolves to `/home/ai-pod/.claude` and gets
    // rejected as a seeded prefix.
    validate_container_path(&target)?;
    Ok(target)
}

/// Path-specific warnings for `mount add`. Returns a list of human-readable
/// reasons the mount is risky; an empty vec means "no concerns".
///
/// This is the place to teach ai-pod about new risky paths — every entry
/// becomes an interactive confirmation gate at `mount add` time.
pub(crate) fn warn_for_spec(spec: &MountSpec, target: &str, home_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let host = spec.host.as_str();

    // 1. Well-known credential dirs / files under $HOME. These are the
    //    obvious sources of host-wide compromise if exposed to the agent.
    let cred_subpaths = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/gh",
        ".config/gcloud",
        ".config/git/credentials",
        ".docker/config.json",
        ".netrc",
        ".kube",
        ".password-store",
    ];
    for sub in cred_subpaths {
        let p = home_dir.join(sub).display().to_string();
        if host == p || host.starts_with(&format!("{}/", p)) {
            out.push(format!(
                "{} holds credentials — the in-container agent would gain full \
                 access to them, including ability to authenticate as you to \
                 remote services.",
                host
            ));
            break;
        }
    }

    // 2. System / runtime paths on the host. These either expose host-wide
    //    state or hand the container a root-equivalent control plane.
    let system_prefixes = [
        "/etc",
        "/var/run",
        "/run",
        "/proc",
        "/sys",
        "/dev",
        "/boot",
        "/root",
        "/var/lib/docker",
        "/var/lib/containers",
    ];
    for sys in system_prefixes {
        if host == sys || host.starts_with(&format!("{}/", sys)) {
            out.push(format!(
                "{} is a host system path — mounting it can leak host secrets \
                 (e.g. /etc/shadow) or grant container-escape primitives (e.g. \
                 /var/run/docker.sock, /dev/*, /proc/*).",
                host
            ));
            break;
        }
    }

    // 3. ai-pod's own config dir. A writable mount here lets a compromised
    //    container rewrite the global mount list for the next launch — a
    //    confused-deputy escalation that compounds with all other risks.
    let ai_pod = home_dir.join(".ai-pod").display().to_string();
    if host == ai_pod || host.starts_with(&format!("{}/", ai_pod)) {
        out.push(format!(
            "{} is ai-pod's own config directory. Mounting it (especially \
             writable) lets a container modify the global mount list and \
             escalate to other host paths on the next launch.",
            host
        ));
    }

    // 4. Container target overrides files that ai-pod itself seeds. The
    //    settings.json hooks carry $AI_POD_API_KEY; .claude.json holds the
    //    MCP URL. Replacing either lets a hostile host file hijack the agent.
    let seeded_files = [
        "/home/ai-pod/.claude/settings.json",
        "/home/ai-pod/.claude.json",
        "/home/ai-pod/.claude/CLAUDE.md",
        "/home/ai-pod/.gitconfig",
    ];
    if seeded_files.contains(&target) {
        out.push(format!(
            "Container target {} is seeded by ai-pod (Stop hook, MCP URL, \
             api-key carrier). Overriding it can hijack the agent's host \
             call-back or silently disable safety hooks.",
            target
        ));
    }

    // 5. Container target inside a system binary / library tree. The
    //    runtime-settings Stop hook is a `curl` invocation; replacing the
    //    binary or libc underneath the agent is full code execution as the
    //    `ai-pod` user with the api key in env.
    let bin_prefixes = [
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/boot",
    ];
    for bp in bin_prefixes {
        if target == bp || target.starts_with(&format!("{}/", bp)) {
            out.push(format!(
                "Container target {} shadows a system binary/library path. \
                 The agent will execute whatever host file you mount there.",
                target
            ));
            break;
        }
    }

    out
}

pub fn run_add(config: &AppConfig, spec_str: &str, writable: bool, assume_yes: bool) -> Result<()> {
    let spec = parse_spec(spec_str, writable, &config.home_dir)?;
    let target = crate::container::resolve_container_target(&spec, &config.home_dir)?;

    let warnings = warn_for_spec(&spec, &target, &config.home_dir);
    if !warnings.is_empty() {
        eprintln!(
            "{} this mount is on ai-pod's risky-path warn-list:",
            "warning:".yellow().bold()
        );
        for w in &warnings {
            eprintln!("  • {}", w);
        }
        if !assume_yes {
            let confirm = dialoguer::Confirm::new()
                .with_prompt(format!(
                    "Proceed with mount {} → {} ({})?",
                    spec.host,
                    target,
                    if spec.writable { "rw" } else { "ro" }
                ))
                .default(false)
                .interact()
                .unwrap_or(false);
            if !confirm {
                anyhow::bail!("Mount cancelled by user");
            }
        }
    }

    let mut gc = GlobalConfig::load(config);

    if let Some(existing) = gc.mounts.iter().find(|m| m.host == spec.host) {
        if existing.writable != spec.writable {
            anyhow::bail!(
                "{} is already mounted as {}. Run `ai-pod mount remove {}` first, \
                 then re-add{}.",
                spec.host,
                if existing.writable { "rw" } else { "ro" },
                spec.host,
                if spec.writable { " with --writable" } else { "" }
            );
        }
        println!("{} {}", "Already mounted:".yellow(), spec.host);
        return Ok(());
    }

    for existing in &gc.mounts {
        let existing_target = match crate::container::resolve_container_target(
            existing,
            &config.home_dir,
        ) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if existing_target == target {
            anyhow::bail!(
                "Container target {} is already used by mount {}; pick a different \
                 container path with `ai-pod mount add {}:<other-path>`.",
                target,
                existing.host,
                spec.host
            );
        }
    }

    gc.add(spec.clone());
    gc.save(config)?;

    println!(
        "{} {} → {} ({})",
        "Mounted:".green().bold(),
        spec.host,
        target,
        if spec.writable { "rw" } else { "ro" }
    );

    warn_if_unreadable(&spec);
    Ok(())
}

pub fn run_remove(config: &AppConfig, host: &str) -> Result<()> {
    let normalized = normalize_host(host, &config.home_dir);
    let mut gc = GlobalConfig::load(config);
    if !gc.remove(&normalized) {
        println!("{} {}", "Not mounted:".yellow(), normalized);
        return Ok(());
    }
    gc.save(config)?;
    println!("{} {}", "Unmounted:".green().bold(), normalized);
    Ok(())
}

pub fn run_list(config: &AppConfig) -> Result<()> {
    let gc = GlobalConfig::load(config);
    if gc.mounts.is_empty() {
        println!(
            "{}",
            "No global mounts configured. Use `ai-pod mount add <host>[:<container>]`."
                .dimmed()
        );
        return Ok(());
    }
    for m in &gc.mounts {
        let target = crate::container::resolve_container_target(m, &config.home_dir)
            .unwrap_or_else(|_| "(invalid)".to_string());
        let mode = if m.writable { "rw" } else { "ro" };
        let exists = if Path::new(&m.host).symlink_metadata().is_ok() {
            ""
        } else {
            "  (missing — will be skipped at launch)"
        };
        println!("{:<50} → {:<40} [{}]{}", m.host, target, mode, exists);
    }
    Ok(())
}

/// One-line warning for `mount add` when the host file is mode-restricted in
/// a way that the in-container `ai-pod` user is unlikely to be able to read
/// it. Best-effort; silent on any error.
fn warn_if_unreadable(spec: &MountSpec) {
    let path = Path::new(&spec.host);
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if !meta.is_file() {
        return;
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o004 != 0 {
        return;
    }
    eprintln!(
        "{} {} has mode {:o}; the container user may not be able to read it under \
         rootless podman. Consider `chmod o+r {}` or rely on docker / rootful podman.",
        "warning:".yellow().bold(),
        spec.host,
        mode,
        spec.host
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_config(home: &Path) -> AppConfig {
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home.to_path_buf(),
        }
    }

    #[test]
    fn parse_spec_host_only() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("abs/path")).unwrap();
        let host = dir.path().join("abs/path");
        let spec = parse_spec(&host.display().to_string(), false, dir.path()).unwrap();
        assert_eq!(spec.host, host.display().to_string());
        assert_eq!(spec.container, None);
        assert!(!spec.writable);
    }

    #[test]
    fn parse_spec_host_and_container() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/abs/a:/abs/b", true, dir.path()).unwrap();
        assert_eq!(spec.host, "/abs/a");
        assert_eq!(spec.container.as_deref(), Some("/abs/b"));
        assert!(spec.writable);
    }

    #[test]
    fn parse_spec_expands_tilde_in_host() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let spec = parse_spec("~/.claude/skills", false, home).unwrap();
        assert_eq!(spec.host, home.join(".claude/skills").display().to_string());
    }

    #[test]
    fn parse_spec_rejects_relative_host() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("relative/path", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn parse_spec_rejects_traversal_in_host() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/a/../b", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn parse_spec_rejects_app_target() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/app/foo", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("/app"));
    }

    #[test]
    fn parse_spec_rejects_app_root_target() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/app", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("/app"));
    }

    #[test]
    fn parse_spec_rejects_reserved_target() {
        let dir = TempDir::new().unwrap();
        for r in ["/proc", "/proc/sys", "/sys", "/dev/null", "/etc/passwd"] {
            let err = parse_spec(&format!("/host:{}", r), false, dir.path()).unwrap_err();
            assert!(
                err.to_string().contains("reserved"),
                "target {} should be reserved",
                r
            );
        }
    }

    #[test]
    fn parse_spec_rejects_container_home_exactly() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/home/ai-pod", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("home volume"));
    }

    #[test]
    fn parse_spec_accepts_container_home_subpath() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/host:/home/ai-pod/.claude/skills", false, dir.path()).unwrap();
        assert_eq!(spec.container.as_deref(), Some("/home/ai-pod/.claude/skills"));
    }

    #[test]
    fn parse_spec_rejects_filesystem_root_host() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/:/host", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("filesystem root"), "got: {err}");
    }

    #[test]
    fn parse_spec_rejects_filesystem_root_host_alone() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("filesystem root"), "got: {err}");
    }

    #[test]
    fn parse_spec_rejects_colon_smuggling_in_container() {
        // `split_once(':')` puts everything after the first colon into the
        // container portion, so this would otherwise smuggle `rw,suid` into
        // the podman -v opts.
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/tmp/x:/foo:rw,suid", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("':'"), "got: {err}");
    }

    #[test]
    fn parse_spec_normalizes_trailing_slash_in_host() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let spec = parse_spec("~/x/", false, home).unwrap();
        assert_eq!(spec.host, home.join("x").display().to_string());
        assert!(!spec.host.ends_with('/'));
    }

    #[test]
    fn parse_spec_normalizes_trailing_slash_in_container() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/host:/foo/bar/", false, dir.path()).unwrap();
        assert_eq!(spec.container.as_deref(), Some("/foo/bar"));
    }

    #[test]
    fn parse_spec_rejects_trailing_slash_bypass_of_container_home() {
        let dir = TempDir::new().unwrap();
        // Without normalization, "/home/ai-pod/" would slip past the
        // `p == CONTAINER_HOME` exact-match check.
        let err = parse_spec("/host:/home/ai-pod/", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("home volume"));
    }

    #[test]
    fn parse_spec_rejects_trailing_slash_bypass_of_app() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/app/", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("/app"));
    }

    #[test]
    fn parse_spec_rejects_trailing_slash_bypass_of_reserved() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/etc/", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn parse_spec_rejects_seeded_dot_claude() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/home/ai-pod/.claude", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("seeded"), "got: {err}");
    }

    #[test]
    fn parse_spec_rejects_seeded_opencode_plugins() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec(
            "/host:/home/ai-pod/.config/opencode/plugins",
            false,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("seeded"), "got: {err}");
    }

    #[test]
    fn parse_spec_rejects_auto_mode_dot_claude() {
        // `~/.claude` in auto-mode resolves to `/home/ai-pod/.claude`, which
        // is a seeded prefix. Without re-validating the resolved target,
        // this would silently shadow the Stop hook + MCP wiring.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let err = parse_spec("~/.claude", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("seeded"), "got: {err}");
    }

    #[test]
    fn parse_spec_rejects_auto_mode_home_root() {
        // `mount add ~` resolves to `/home/ai-pod`, shadowing the home volume.
        let dir = TempDir::new().unwrap();
        let err = parse_spec("~", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("home volume"), "got: {err}");
    }

    #[test]
    fn normalize_host_only_at_start() {
        let home = Path::new("/H");
        assert_eq!(normalize_host("~", home), "/H");
        assert_eq!(normalize_host("~/foo", home), "/H/foo");
        assert_eq!(normalize_host("~/foo/", home), "/H/foo");
        assert_eq!(normalize_host("/abs/~/foo", home), "/abs/~/foo");
        assert_eq!(normalize_host("/abs", home), "/abs");
        assert_eq!(normalize_host("/", home), "/");
    }

    #[test]
    fn run_add_rejects_no_container_outside_home() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        let err = run_add(&config, "/etc/foo", false, true).unwrap_err();
        assert!(err.to_string().contains("outside $HOME"));
    }

    #[test]
    fn run_add_and_remove_round_trip() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();

        run_add(&config, "~/.claude/skills", false, true).unwrap();
        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1);
        assert_eq!(
            gc.mounts[0].host,
            dir.path().join(".claude/skills").display().to_string()
        );

        run_remove(
            &config,
            &dir.path().join(".claude/skills").display().to_string(),
        )
        .unwrap();
        let gc = GlobalConfig::load(&config);
        assert!(gc.mounts.is_empty());
    }

    #[test]
    fn run_remove_normalizes_trailing_slash() {
        // Symmetric with parse_spec normalization: a user who types
        // `~/x/` for `mount remove` should find the entry stored as `~/x`.
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();
        run_add(&config, "~/.claude/skills", false, true).unwrap();

        run_remove(&config, "~/.claude/skills/").unwrap();
        let gc = GlobalConfig::load(&config);
        assert!(gc.mounts.is_empty(), "remove should find the entry");
    }

    #[test]
    fn run_add_dedups_by_host_when_writable_matches() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();

        run_add(&config, "~/.claude/skills", false, true).unwrap();
        run_add(&config, "~/.claude/skills", false, true).unwrap();
        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1, "duplicate host should not be added");
        assert!(!gc.mounts[0].writable);
    }

    #[test]
    fn run_add_errors_on_writable_mismatch() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();

        run_add(&config, "~/.claude/skills", false, true).unwrap();
        let err = run_add(&config, "~/.claude/skills", true, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already mounted"), "got: {msg}");
        assert!(msg.contains("remove"), "should hint to remove first: {msg}");

        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1);
        assert!(!gc.mounts[0].writable, "stored entry should be unchanged");
    }

    fn spec(host: &str, container: Option<&str>) -> MountSpec {
        MountSpec {
            host: host.to_string(),
            container: container.map(|c| c.to_string()),
            writable: false,
        }
    }

    #[test]
    fn warn_flags_ssh_aws_gnupg_under_home() {
        let home = Path::new("/H");
        for sub in [".ssh", ".aws", ".gnupg", ".config/gh", ".netrc"] {
            let host = format!("/H/{}", sub);
            let target = format!("/home/ai-pod/{}", sub);
            let w = warn_for_spec(&spec(&host, Some(&target)), &target, home);
            assert!(!w.is_empty(), "{} should warn", sub);
            assert!(w.iter().any(|m| m.contains("credentials")), "got {:?}", w);
        }
    }

    #[test]
    fn warn_flags_system_host_paths() {
        let home = Path::new("/H");
        for sys in [
            "/etc/hosts",
            "/var/run/docker.sock",
            "/dev/null",
            "/proc/1",
            "/sys/kernel",
            "/root/x",
        ] {
            let w = warn_for_spec(&spec(sys, Some("/x")), "/x", home);
            assert!(!w.is_empty(), "{} should warn", sys);
            assert!(w.iter().any(|m| m.contains("system path")), "got {:?}", w);
        }
    }

    #[test]
    fn warn_flags_ai_pod_self_mount() {
        let home = Path::new("/H");
        let w = warn_for_spec(&spec("/H/.ai-pod", None), "/home/ai-pod/.ai-pod", home);
        assert!(w.iter().any(|m| m.contains("ai-pod's own")), "got {:?}", w);

        let w = warn_for_spec(
            &spec("/H/.ai-pod/config.json", Some("/home/ai-pod/cfg")),
            "/home/ai-pod/cfg",
            home,
        );
        assert!(w.iter().any(|m| m.contains("ai-pod's own")), "got {:?}", w);
    }

    #[test]
    fn warn_flags_seeded_target_files() {
        let home = Path::new("/H");
        for t in [
            "/home/ai-pod/.claude/settings.json",
            "/home/ai-pod/.claude.json",
            "/home/ai-pod/.claude/CLAUDE.md",
        ] {
            let w = warn_for_spec(&spec("/H/x", Some(t)), t, home);
            assert!(
                w.iter().any(|m| m.contains("seeded by ai-pod")),
                "{} should warn, got {:?}",
                t,
                w
            );
        }
    }

    #[test]
    fn warn_flags_binary_shadow_targets() {
        let home = Path::new("/H");
        for t in [
            "/usr/bin/curl",
            "/bin/sh",
            "/sbin/init",
            "/usr/local/bin/foo",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/boot/vmlinuz",
        ] {
            let w = warn_for_spec(&spec("/H/x", Some(t)), t, home);
            assert!(
                w.iter().any(|m| m.contains("system binary/library")),
                "{} should warn, got {:?}",
                t,
                w
            );
        }
    }

    #[test]
    fn warn_silent_on_benign_skills_mount() {
        let home = Path::new("/H");
        let w = warn_for_spec(
            &spec("/H/.claude/skills", None),
            "/home/ai-pod/.claude/skills",
            home,
        );
        assert!(w.is_empty(), "skills mount should be silent, got {:?}", w);
    }

    #[test]
    fn run_add_aborts_risky_mount_without_yes() {
        // Non-interactive: dialoguer::Confirm falls back to its default
        // (false), so the add must bail rather than succeed silently.
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();

        let err = run_add(&config, &ssh.display().to_string(), false, false).unwrap_err();
        assert!(
            err.to_string().contains("cancelled"),
            "expected cancellation, got: {}",
            err
        );
        let gc = GlobalConfig::load(&config);
        assert!(gc.mounts.is_empty(), "risky mount must not be stored");
    }

    #[test]
    fn run_add_allows_risky_mount_with_yes() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();

        run_add(&config, &ssh.display().to_string(), false, true).unwrap();
        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1);
    }

    #[test]
    fn run_add_rejects_colliding_container_target() {
        let dir = TempDir::new().unwrap();
        let config = make_config(dir.path());
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        let a = dir.path().join("a").display().to_string();
        let b = dir.path().join("b").display().to_string();

        run_add(&config, &format!("{}:/home/ai-pod/shared", a), false, true).unwrap();
        let err = run_add(&config, &format!("{}:/home/ai-pod/shared", b), false, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already used"), "got: {msg}");

        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1, "colliding mount should not be stored");
    }
}
