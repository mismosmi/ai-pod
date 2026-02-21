use colored::Colorize;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/farbenmeer/ai-pod/releases/latest";

pub async fn check_for_update() {
    if let Ok(latest) = fetch_latest_version().await {
        if is_newer(&latest, CURRENT_VERSION) {
            println!(
                "{} {} → {} — {}",
                "Update available:".yellow().bold(),
                CURRENT_VERSION.dimmed(),
                latest.green().bold(),
                "https://github.com/farbenmeer/ai-pod/releases/latest"
            );
        }
    }
}

async fn fetch_latest_version() -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .user_agent(format!("ai-pod/{CURRENT_VERSION}"))
        .build()?;

    let resp: serde_json::Value = client
        .get(RELEASES_URL)
        .send()
        .await?
        .json()
        .await?;

    let tag = resp["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tag_name"))?;

    Ok(tag.trim_start_matches('v').to_string())
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut parts = v.splitn(3, '.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn newer_patch() {
        assert!(is_newer("0.2.2", "0.2.1"));
    }

    #[test]
    fn newer_minor() {
        assert!(is_newer("0.3.0", "0.2.9"));
    }

    #[test]
    fn newer_major() {
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn same_version() {
        assert!(!is_newer("0.2.1", "0.2.1"));
    }

    #[test]
    fn older_version() {
        assert!(!is_newer("0.2.0", "0.2.1"));
    }
}
