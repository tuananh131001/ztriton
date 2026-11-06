//! Live Jellyfin credentials for the end-to-end tests.
//!
//! Each value comes from the environment if set, otherwise from the
//! git-ignored `.env` at the repository root (see `.env.example`), so the
//! tests run the same from `cargo test`, `make test` or an editor.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub struct Credentials {
    pub url: String,
    pub user: String,
    pub password: String,
}

/// `Ok(None)` when no server is configured, so callers can skip.
pub fn load() -> Result<Option<Credentials>> {
    let file = read_env_file(&Path::new(env!("CARGO_MANIFEST_DIR")).join(".env"))?;
    let value = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());
    let Some(url) = value("JELLYFIN_URL").filter(|url| !url.is_empty()) else {
        return Ok(None);
    };
    let user = value("JELLYFIN_USER").context("JELLYFIN_URL is set but JELLYFIN_USER is not")?;
    let password = value("JELLYFIN_PASSWORD").unwrap_or_default();
    Ok(Some(Credentials { url, user, password }))
}

/// `KEY=value` lines; blank lines and `#` comments are ignored, and one pair
/// of surrounding quotes is stripped so values may contain spaces.
fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let key = key.trim().trim_start_matches("export ").trim();
            let value = value.trim();
            let unquoted = ['"', '\''].iter().find_map(|quote| {
                value.strip_prefix(*quote).and_then(|v| v.strip_suffix(*quote))
            });
            (key.to_string(), unquoted.unwrap_or(value).to_string())
        })
        .collect())
}
