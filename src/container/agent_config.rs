//! Agent settings, MCP configuration, and general browser-access guidance.

use crate::config::AppConfig;
use anyhow::{Context, Result};

pub(super) fn generate_runtime_settings(config: &AppConfig) -> Result<()> {
    let mut settings: serde_json::Value = if config.claude_settings_path().exists() {
        let raw = std::fs::read_to_string(config.claude_settings_path())
            .context("Failed to read settings.json")?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let notify_curl = |msg: &str| {
        format!(
            "curl -fsS -X POST -H \"X-Api-Key: $AI_POD_API_KEY\" -H 'Content-Type: application/json' -d '{{\"project_id\":\"'\"$AI_POD_PROJECT_ID\"'\",\"message\":\"{}\"}}' \"$AI_POD_SERVER_URL/notify_user\" >/dev/null || true",
            msg
        )
    };

    let stop_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": notify_curl("Task completed"),
        }]
    }]);

    let permission_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": notify_curl("Claude needs your approval"),
        }]
    }]);

    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;

    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().context("hooks is not an object")?;
    hooks_obj.insert("Stop".to_string(), stop_hook);
    hooks_obj.insert("PermissionRequest".to_string(), permission_hook);

    // Set default permission mode — no per-tool prompts in TUI
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let perms_obj = permissions
        .as_object_mut()
        .context("permissions is not an object")?;
    perms_obj.insert(
        "defaultMode".to_string(),
        serde_json::Value::String("bypassPermissions".to_string()),
    );

    let output = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&config.runtime_settings, output).context("Failed to write runtime settings")?;

    Ok(())
}

/// Persistent Claude configuration resolves credentials from the main
/// container's environment, so each session needs no home-file rewrite.
pub(super) fn claude_mcp_entry() -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": "${AI_POD_SERVER_URL}/mcp",
        "headers": {
            "X-Api-Key": "${AI_POD_API_KEY}",
            "X-Ai-Pod-Session-Id": "${AI_POD_SESSION_ID}",
        }
    })
}

/// MCP server entry for the host-side Playwright server, as consumed by Claude
/// Code and OpenCode. The url points at the runtime's host gateway; the
/// spoofed `Host` header satisfies Playwright MCP's host allow-list (see
/// [`crate::playwright`]).
fn playwright_mcp_entry(host_gateway: &str, opencode: bool) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "type": if opencode { "remote" } else { "http" },
        "url": crate::playwright::mcp_url(host_gateway),
        "headers": {
            "Host": crate::playwright::host_header(),
        }
    });
    if opencode {
        entry["enabled"] = serde_json::Value::Bool(true);
    }
    entry
}

/// Full inline config injected into OpenCode via the `OPENCODE_CONFIG_CONTENT`
/// env var. Since the env var is set per-launch, we can bake the literal
/// values in directly — no interpolation needed.
pub(super) fn opencode_config_content(
    server_url: &str,
    api_key: &str,
    session_id: &str,
    playwright: Option<&str>,
) -> String {
    let mut value = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": {
            "ai-pod": {
                "type": "remote",
                "url": format!("{}/mcp", server_url),
                "enabled": true,
                "headers": {
                    "X-Api-Key": api_key,
                    "X-Ai-Pod-Session-Id": session_id,
                }
            }
        }
    });
    if let Some(host_gateway) = playwright {
        value["mcp"]["playwright"] = playwright_mcp_entry(host_gateway, true);
    }
    serde_json::to_string(&value).expect("serialize opencode config content")
}

/// Merge ai-pod's keys into a Codex `config.toml`, preserving everything else.
///
/// Codex reads `~/.codex/config.toml` (TOML); login/auth lives separately in
/// `~/.codex/auth.json` and is never touched here. We use `toml_edit` so a
/// user's persisted, possibly hand-edited config (model, provider, comments,
/// other `[mcp_servers.*]`) survives byte-for-byte — only the five keys we own
/// are installed on creation/version changes. Credentials resolve from the
/// environment at runtime:
///   - `experimental_use_rmcp_client` (enables native streamable-HTTP MCP)
///   - `[mcp_servers.ai-pod]` (url + env_http_headers)
///   - `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
///     (the container is the sandbox, matching Claude's `bypassPermissions`
///     and OpenCode's YOLO mode; a user's stricter value is intentionally
///     overridden when versioned defaults are installed)
///   - `notify` (points at the in-container notify helper)
pub(super) fn codex_config_merge(
    existing: &str,
    server_url: &str,
    host_gateway: &str,
    playwright: bool,
) -> String {
    // Missing OR unparsable config -> start from an empty document.
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();

    doc["experimental_use_rmcp_client"] = toml_edit::value(true);
    doc["approval_policy"] = toml_edit::value("never");
    doc["sandbox_mode"] = toml_edit::value("danger-full-access");

    let mut notify = toml_edit::Array::new();
    notify.push("/usr/local/bin/ai-pod-codex-notify");
    doc["notify"] = toml_edit::value(notify);

    update_codex_connection(&mut doc, server_url, host_gateway, playwright);

    doc.to_string()
}

fn update_codex_connection(
    doc: &mut toml_edit::DocumentMut,
    server_url: &str,
    host_gateway: &str,
    playwright: bool,
) {
    let server = &mut doc["mcp_servers"]["ai-pod"];
    server["url"] = toml_edit::value(format!("{}/mcp", server_url));
    // Migrate literal headers written by older ai-pod versions without
    // discarding unrelated headers a user added.
    if let Some(headers) = server
        .get_mut("http_headers")
        .and_then(|v| v.as_table_like_mut())
    {
        headers.remove("X-Api-Key");
        headers.remove("X-Ai-Pod-Session-Id");
    }
    server["env_http_headers"]["X-Api-Key"] = toml_edit::value("AI_POD_API_KEY");
    server["env_http_headers"]["X-Ai-Pod-Session-Id"] = toml_edit::value("AI_POD_SESSION_ID");
    sync_codex_playwright(doc, host_gateway, playwright);
}

fn managed_playwright_url(url: Option<&str>) -> bool {
    url.is_some_and(|url| {
        ["host.containers.internal", "host.docker.internal"]
            .iter()
            .any(|host| url == crate::playwright::mcp_url(host))
    })
}

pub(super) fn sync_claude_playwright(
    servers: &mut serde_json::Map<String, serde_json::Value>,
    host: &str,
    enabled: bool,
) {
    if enabled {
        servers.insert("playwright".into(), playwright_mcp_entry(host, false));
    } else if managed_playwright_url(servers.get("playwright").and_then(|v| v["url"].as_str())) {
        servers.remove("playwright");
    }
}

/// Compare owned fields semantically so custom formatting and unrelated settings
/// do not cause a rewrite on every launch.
pub(super) fn sync_codex_playwright(
    doc: &mut toml_edit::DocumentMut,
    host: &str,
    enabled: bool,
) -> bool {
    let current = doc.get("mcp_servers").and_then(|v| v.get("playwright"));
    if !enabled {
        if managed_playwright_url(current.and_then(|v| v.get("url")).and_then(|v| v.as_str())) {
            doc.get_mut("mcp_servers")
                .and_then(|v| v.as_table_like_mut())
                .unwrap()
                .remove("playwright");
            return true;
        }
        return false;
    }
    let url = crate::playwright::mcp_url(host);
    let header = crate::playwright::host_header();
    if current.and_then(|v| v.get("url")).and_then(|v| v.as_str()) == Some(url.as_str())
        && current
            .and_then(|v| v.get("http_headers"))
            .and_then(|v| v.get("Host"))
            .and_then(|v| v.as_str())
            == Some(header.as_str())
        && current
            .and_then(|v| v.get("enabled"))
            .map(|v| v.as_bool() == Some(true))
            .unwrap_or(true)
    {
        return false;
    }
    let server = &mut doc["mcp_servers"]["playwright"];
    server["url"] = toml_edit::value(url);
    server["http_headers"]["Host"] = toml_edit::value(header);
    server["enabled"] = toml_edit::value(true);
    true
}

const PLAYWRIGHT_GUIDANCE: &str = "If you need browser access and Playwright MCP is unavailable, ask the user to run `ai-pod --playwright`.";

pub(super) fn with_playwright_guidance(existing: &str) -> String {
    let mut content = existing.to_string();
    if !content.contains(PLAYWRIGHT_GUIDANCE) {
        if !content.is_empty() {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
        }
        content.push_str(PLAYWRIGHT_GUIDANCE);
        content.push('\n');
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn runtime_settings_contains_stop_hook() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();

        let stop = &json["hooks"]["Stop"];
        assert!(stop.is_array(), "hooks.Stop should be an array");
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("notify_user"));
        assert!(cmd.contains("$AI_POD_SERVER_URL"));
    }

    #[test]
    fn runtime_settings_stop_hook_uses_curl() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let cmd = json["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.starts_with("curl"));
    }

    #[test]
    fn runtime_settings_contains_default_mode_bypass_permissions() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["permissions"]["defaultMode"], "bypassPermissions");
    }

    #[test]
    fn runtime_settings_does_not_contain_mcp_servers() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(json["mcpServers"].is_null(), "mcpServers should not be set");
    }

    #[test]
    fn runtime_settings_preserves_existing_keys() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);

        let claude_dir = config.home_dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let existing = serde_json::json!({"theme": "dark", "verbosity": "verbose"});
        std::fs::write(
            config.claude_settings_path(),
            serde_json::to_string(&existing).unwrap(),
        )
        .unwrap();

        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["theme"], "dark");
        assert_eq!(json["verbosity"], "verbose");
    }

    #[test]
    fn claude_mcp_entry_uses_environment() {
        let entry = claude_mcp_entry();
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "${AI_POD_SERVER_URL}/mcp");
        assert_eq!(entry["headers"]["X-Api-Key"], "${AI_POD_API_KEY}");
        assert_eq!(
            entry["headers"]["X-Ai-Pod-Session-Id"],
            "${AI_POD_SESSION_ID}"
        );
    }

    #[test]
    fn opencode_config_content_bakes_literal_values() {
        let s = opencode_config_content("http://host.containers.internal:7822", "k1", "s2", None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mcp"]["ai-pod"]["type"], "remote");
        assert_eq!(
            v["mcp"]["ai-pod"]["url"],
            "http://host.containers.internal:7822/mcp"
        );
        assert_eq!(v["mcp"]["ai-pod"]["enabled"], true);
        assert_eq!(v["mcp"]["ai-pod"]["headers"]["X-Api-Key"], "k1");
        assert_eq!(v["mcp"]["ai-pod"]["headers"]["X-Ai-Pod-Session-Id"], "s2");
    }

    #[test]
    fn opencode_config_includes_playwright_when_enabled() {
        let s = opencode_config_content(
            "http://host.containers.internal:7822",
            "k1",
            "s2",
            Some("host.containers.internal"),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mcp"]["playwright"]["type"], "remote");
        assert_eq!(v["mcp"]["playwright"]["enabled"], true);
        assert_eq!(
            v["mcp"]["playwright"]["url"],
            "http://host.containers.internal:8931/mcp"
        );
        assert_eq!(v["mcp"]["playwright"]["headers"]["Host"], "localhost:8931");
    }

    #[test]
    fn claude_playwright_entry_is_http_config() {
        let entry = playwright_mcp_entry("host.containers.internal", false);
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://host.containers.internal:8931/mcp");
        assert_eq!(entry["headers"]["Host"], "localhost:8931");
        assert!(entry["enabled"].is_null());
    }

    #[test]
    fn codex_config_enables_playwright_when_requested() {
        let existing = "[mcp_servers.playwright]\nurl = \"http://old-host/mcp\"\nenabled = false\n";
        let out = codex_config_merge(
            existing,
            "http://host.containers.internal:7822",
            "host.containers.internal",
            true,
        );
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        let server = &doc["mcp_servers"]["playwright"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.containers.internal:8931/mcp")
        );
        assert_eq!(
            server["http_headers"]["Host"].as_str(),
            Some("localhost:8931")
        );
        assert_eq!(server["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn playwright_config_is_omitted_and_removed_when_disabled() {
        let opencode: serde_json::Value = serde_json::from_str(&opencode_config_content(
            "http://host",
            "key",
            "session",
            None,
        ))
        .unwrap();
        assert!(opencode["mcp"]["playwright"].is_null());
        let mut servers = serde_json::Map::new();
        servers.insert(
            "other".into(),
            serde_json::json!({"url":"http://other/mcp"}),
        );
        sync_claude_playwright(&mut servers, "host.containers.internal", true);
        sync_claude_playwright(&mut servers, "host.containers.internal", false);
        assert!(!servers.contains_key("playwright"));
        assert!(servers.contains_key("other"));
        let mut doc = codex_config_merge(
            "model = \"mine\"\n",
            "http://host",
            "host.containers.internal",
            true,
        )
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
        assert!(sync_codex_playwright(
            &mut doc,
            "host.containers.internal",
            false
        ));
        assert!(doc["mcp_servers"].get("playwright").is_none());
        assert_eq!(doc["model"].as_str(), Some("mine"));
        assert!(!sync_codex_playwright(
            &mut doc,
            "host.containers.internal",
            false
        ));
    }

    #[test]
    fn matching_playwright_config_keeps_custom_formatting_and_headers() {
        let input = "# keep this comment\n[mcp_servers.playwright]\nurl='http://host.containers.internal:8931/mcp'\nenabled=true\n[mcp_servers.playwright.http_headers]\nHost='localhost:8931'\nOther='keep'\n";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!sync_codex_playwright(
            &mut doc,
            "host.containers.internal",
            true
        ));
        assert_eq!(doc.to_string(), input);
        let implicit = input.replace("enabled=true\n", "");
        let mut doc = implicit.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!sync_codex_playwright(
            &mut doc,
            "host.containers.internal",
            true
        ));
        assert_eq!(doc.to_string(), implicit);
        let mut servers = serde_json::Map::new();
        servers.insert(
            "playwright".into(),
            serde_json::json!({"url":"http://personal/mcp"}),
        );
        sync_claude_playwright(&mut servers, "host.containers.internal", false);
        assert!(servers.contains_key("playwright"));
    }

    #[test]
    fn playwright_guidance_preserves_instructions_without_duplication() {
        let existing = "# Personal instructions\nKeep my preferred style.\n";
        let updated = with_playwright_guidance(existing);
        assert!(updated.starts_with(existing));
        assert!(updated.contains(PLAYWRIGHT_GUIDANCE));
        assert_eq!(with_playwright_guidance(&updated), updated);
        assert_eq!(
            with_playwright_guidance(""),
            format!("{PLAYWRIGHT_GUIDANCE}\n")
        );
    }

    #[test]
    fn codex_config_merge_into_empty() {
        let out = codex_config_merge(
            "",
            "http://host.containers.internal:7822",
            "host.containers.internal",
            true,
        );
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["experimental_use_rmcp_client"].as_bool(), Some(true));
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(doc["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert_eq!(
            doc["notify"].as_array().unwrap().get(0).unwrap().as_str(),
            Some("/usr/local/bin/ai-pod-codex-notify")
        );
        let server = &doc["mcp_servers"]["ai-pod"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.containers.internal:7822/mcp")
        );
        assert_eq!(
            server["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
        assert_eq!(
            server["env_http_headers"]["X-Ai-Pod-Session-Id"].as_str(),
            Some("AI_POD_SESSION_ID")
        );
    }

    #[test]
    fn codex_config_merge_preserves_existing_keys() {
        let existing = "model = \"gpt-5\"\n# my comment\n";
        let out = codex_config_merge(
            existing,
            "http://host.containers.internal:7822",
            "host.containers.internal",
            true,
        );
        // The user's key and comment survive (toml_edit, not a typed rewrite).
        assert!(
            out.contains("# my comment"),
            "comment should survive: {out}"
        );
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        // ...and ai-pod's keys are still merged in.
        assert_eq!(
            doc["mcp_servers"]["ai-pod"]["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
    }

    #[test]
    fn codex_config_merge_url_uses_runtime_gateway() {
        // On Docker the gateway is host.docker.internal — the url must reflect
        // whatever server_url is passed, not a hardcoded host.
        let out = codex_config_merge(
            "",
            "http://host.docker.internal:7822",
            "host.docker.internal",
            true,
        );
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["mcp_servers"]["ai-pod"]["url"].as_str(),
            Some("http://host.docker.internal:7822/mcp")
        );
    }

    #[test]
    fn codex_config_migrates_literal_credentials() {
        let existing = "[mcp_servers.ai-pod.http_headers]\nX-Api-Key = \"old-key\"\nX-Ai-Pod-Session-Id = \"old-session\"\nOther = \"preserved\"\n";
        let output =
            codex_config_merge(existing, "http://h:7822", "host.containers.internal", true);
        let doc = output.parse::<toml_edit::DocumentMut>().unwrap();
        let server = &doc["mcp_servers"]["ai-pod"];
        assert_eq!(server["http_headers"]["Other"].as_str(), Some("preserved"));
        assert_eq!(
            server["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
        assert!(!output.contains("old-key"));
        assert!(!output.contains("old-session"));
    }
}
