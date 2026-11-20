//! Workspace secrets sealed with SOPS, decrypted on the host at launch.
//!
//! `ai-pod.secrets.env` in the workspace root (or the file named by
//! `AI_POD_SECRETS_FILE`) is a SOPS-encrypted dotenv file. It is safe to commit:
//! values are encrypted, and teammates are added as recipients (age, KMS, Vault...).
//!
//! At launch ai-pod runs `sops --decrypt` on the host and reads the plaintext from
//! the pipe; it is never written to disk. Each value is set on the container
//! runtime's own process environment and only `-e NAME` goes on the command line,
//! so values never appear in argv, `--dry-run` output or logs. The SOPS key
//! (`SOPS_AGE_KEY`, `SOPS_AGE_KEY_FILE`, cloud credentials) stays on the host and is
//! never passed into the container.
//!
//! If the file exists but cannot be decrypted, the launch fails rather than
//! starting the agent without its secrets.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Env var naming the sealed secrets file (absolute, or relative to the workspace).
pub const SECRETS_FILE_VAR: &str = "AI_POD_SECRETS_FILE";
/// Default sealed secrets file in the workspace root (next to `ai-pod.Dockerfile`).
pub const DEFAULT_SECRETS_FILE: &str = "ai-pod.secrets.env";
/// Env var overriding the `sops` binary (mainly for tests and unusual installs).
pub const SOPS_BIN_VAR: &str = "AI_POD_SOPS_BIN";

/// Decrypted name/value pairs. `Debug` prints names only.
pub struct SealedSecrets {
    file: PathBuf,
    vars: Vec<(String, String)>,
}

impl std::fmt::Debug for SealedSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedSecrets")
            .field("file", &self.file)
            .field("names", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

impl SealedSecrets {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.iter().map(|(k, _)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Put each value on the runtime command's environment and return the
    /// matching `-e NAME` arguments (names only).
    pub fn apply(&self, cmd: &mut Command) -> Vec<String> {
        let mut args = Vec::with_capacity(self.vars.len() * 2);
        for (name, value) in &self.vars {
            cmd.env(name, value);
            args.push("-e".to_string());
            args.push(name.clone());
        }
        args
    }

    /// Drop `-e NAME` pairs from `args` whose NAME is provided by these secrets,
    /// so a name is passed once and the sealed value wins over the host's.
    pub fn without_overridden(&self, args: Vec<String>) -> Vec<String> {
        let mut out = Vec::with_capacity(args.len());
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if a == "-e"
                && let Some(name) = it.next()
            {
                if !self.vars.iter().any(|(k, _)| *k == name) {
                    out.push(a);
                    out.push(name);
                }
                continue;
            }
            out.push(a);
        }
        out
    }
}

/// Load and decrypt the workspace's sealed secrets, if any.
///
/// `Ok(None)` when there is no secrets file; an error when the file exists (or was
/// named explicitly) but cannot be decrypted.
pub fn load(workspace: &Path) -> Result<Option<SealedSecrets>> {
    let explicit = std::env::var(SECRETS_FILE_VAR)
        .ok()
        .filter(|v| !v.is_empty());
    let Some(file) = resolve(workspace, explicit.as_deref())? else {
        return Ok(None);
    };
    let bin = std::env::var_os(SOPS_BIN_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("sops"));
    decrypt_with(&bin, &file).map(Some)
}

fn resolve(workspace: &Path, explicit: Option<&str>) -> Result<Option<PathBuf>> {
    match explicit {
        Some(p) => {
            let p = PathBuf::from(p);
            let p = if p.is_absolute() {
                p
            } else {
                workspace.join(p)
            };
            if !p.is_file() {
                bail!(
                    "{SECRETS_FILE_VAR} points to {}, which does not exist",
                    p.display()
                );
            }
            Ok(Some(p))
        }
        None => {
            let p = workspace.join(DEFAULT_SECRETS_FILE);
            Ok(p.is_file().then_some(p))
        }
    }
}

fn decrypt_with(bin: &Path, file: &Path) -> Result<SealedSecrets> {
    let output = Command::new(bin)
        .args([
            "--decrypt",
            "--input-type",
            "dotenv",
            "--output-type",
            "dotenv",
        ])
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "{} exists but `sops` is not installed (https://github.com/getsops/sops); \
                     refusing to start without its secrets",
                    file.display()
                )
            } else {
                anyhow!("failed to run sops: {e}")
            }
        })?;
    if !output.status.success() {
        // sops errors describe keys and metadata, never plaintext values.
        let err = String::from_utf8_lossy(&output.stderr);
        bail!(
            "sops could not decrypt {}: {}\n\
             Is your key available on the host (e.g. SOPS_AGE_KEY or SOPS_AGE_KEY_FILE)? \
             Is the file encrypted (sops -e -i {})?",
            file.display(),
            err.trim(),
            file.display()
        );
    }
    let text = String::from_utf8(output.stdout).context("decrypted secrets are not valid UTF-8")?;
    let vars = parse_dotenv(&text).with_context(|| format!("in {}", file.display()))?;
    Ok(SealedSecrets {
        file: file.to_path_buf(),
        vars,
    })
}

/// Parse `NAME=value` lines (comments, blank lines and `export ` prefixes allowed).
/// Values are taken verbatim; the last definition of a name wins. Errors name the
/// line number only, never the value.
fn parse_dotenv(text: &str) -> Result<Vec<(String, String)>> {
    let mut vars: Vec<(String, String)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((name, value)) = trimmed.split_once('=') else {
            bail!("line {}: expected NAME=value", i + 1);
        };
        let name = name.trim();
        if !is_valid_env_name(name) {
            bail!("line {}: invalid variable name", i + 1);
        }
        if let Some(slot) = vars.iter_mut().find(|(k, _)| k == name) {
            slot.1 = value.to_string();
        } else {
            vars.push((name.to_string(), value.to_string()));
        }
    }
    Ok(vars)
}

pub(super) fn is_valid_env_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_sops(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("sops");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn parses_names_values_comments_and_export() {
        let v = parse_dotenv("# c\n\nA=1\nexport B=x=y\r\nC=\nA=2\n").unwrap();
        assert_eq!(
            v,
            vec![
                ("A".into(), "2".into()),
                ("B".into(), "x=y".into()),
                ("C".into(), String::new())
            ]
        );
    }

    #[test]
    fn rejects_bad_lines_without_echoing_values() {
        let e = parse_dotenv("GOOD=1\nnot a pair\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 2"), "{e}");
        let e = parse_dotenv("1BAD=supersecret\n").unwrap_err().to_string();
        assert!(e.contains("line 1") && !e.contains("supersecret"), "{e}");
    }

    #[test]
    fn resolves_default_and_explicit_files() {
        let ws = tempfile::tempdir().unwrap();
        assert!(resolve(ws.path(), None).unwrap().is_none());
        std::fs::write(ws.path().join(DEFAULT_SECRETS_FILE), "x").unwrap();
        assert_eq!(
            resolve(ws.path(), None).unwrap().unwrap(),
            ws.path().join(DEFAULT_SECRETS_FILE)
        );
        std::fs::write(ws.path().join("team.env"), "x").unwrap();
        assert_eq!(
            resolve(ws.path(), Some("team.env")).unwrap().unwrap(),
            ws.path().join("team.env")
        );
        assert!(resolve(ws.path(), Some("missing.env")).is_err());
    }

    #[test]
    fn decrypts_through_sops_and_applies_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_sops(
            dir.path(),
            "printf 'API_TOKEN=s3cr3t\\nDB_URL=postgres://u:p@h/db\\n'",
        );
        let file = dir.path().join("ai-pod.secrets.env");
        std::fs::write(&file, "encrypted").unwrap();
        let s = decrypt_with(&bin, &file).unwrap();
        assert_eq!(s.names().collect::<Vec<_>>(), ["API_TOKEN", "DB_URL"]);
        assert!(!format!("{s:?}").contains("s3cr3t"));

        let mut cmd = Command::new("true");
        let args = s.apply(&mut cmd);
        assert_eq!(args, ["-e", "API_TOKEN", "-e", "DB_URL"]);
        assert!(
            args.iter().all(|a| !a.contains('=')),
            "values must never reach argv"
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "API_TOKEN" && *v == Some("s3cr3t".as_ref()))
        );
    }

    #[test]
    fn decrypt_failure_and_missing_sops_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ai-pod.secrets.env");
        std::fs::write(&file, "x").unwrap();
        let bad = fake_sops(dir.path(), "echo 'Failed to get the data key' >&2; exit 1");
        let e = decrypt_with(&bad, &file).unwrap_err().to_string();
        assert!(
            e.contains("could not decrypt") && e.contains("data key"),
            "{e}"
        );
        let e = decrypt_with(&dir.path().join("no-such-sops"), &file)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not installed"), "{e}");
    }

    #[test]
    fn sealed_values_override_passthrough_names() {
        let s = SealedSecrets {
            file: PathBuf::from("x"),
            vars: vec![("API_TOKEN".into(), "v".into())],
        };
        let args = vec!["-e", "API_TOKEN", "-e", "GITHUB_TOKEN"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(s.without_overridden(args), ["-e", "GITHUB_TOKEN"]);
    }
}
