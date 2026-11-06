//! Saved login, stored in the macOS login Keychain (never in a file).
//!
//! One generic-password item (service [`service()`], account `"session"`)
//! holds a small JSON blob: server URL, username and — when "Remember me"
//! was ticked — the Jellyfin access token. The plaintext password is never
//! persisted; the token is what lets the app sign back in, and signing out
//! revokes it on the server and removes it from the Keychain.

use anyhow::{Context as _, Result};
use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};
use serde::{Deserialize, Serialize};

const ACCOUNT: &str = "session";
/// `errSecItemNotFound`
const NOT_FOUND: i32 = -25300;

/// Keychain service name. `ZTRITON_KEYCHAIN_SERVICE` overrides it so tests
/// never touch the real app entry.
pub fn service() -> String {
    std::env::var("ZTRITON_KEYCHAIN_SERVICE").unwrap_or_else(|_| "dev.ztriton.player".into())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLogin {
    pub server: String,
    pub username: String,
    /// Present only while signed in with "Remember me".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SavedSession>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    pub user_id: String,
    pub token: String,
}

impl std::fmt::Debug for SavedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedSession").field("user_id", &self.user_id).field("token", &"<redacted>").finish()
    }
}

/// Read the saved login. `Ok(None)` when nothing is stored.
pub fn load() -> Result<Option<SavedLogin>> {
    match get_generic_password(&service(), ACCOUNT) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context("corrupt Keychain entry")?)),
        Err(e) if e.code() == NOT_FOUND => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Keychain read failed: {e}")),
    }
}

/// Create or replace the saved login.
pub fn save(login: &SavedLogin) -> Result<()> {
    let json = serde_json::to_vec(login)?;
    set_generic_password(&service(), ACCOUNT, &json).map_err(|e| anyhow::anyhow!("Keychain write failed: {e}"))
}

/// Remove the saved login entirely. Succeeds if nothing was stored.
pub fn delete() -> Result<()> {
    match delete_generic_password(&service(), ACCOUNT) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == NOT_FOUND => Ok(()),
        Err(e) => Err(anyhow::anyhow!("Keychain delete failed: {e}")),
    }
}

/// Drop the token but keep server + username so the login form is prefilled.
pub fn forget_session() -> Result<()> {
    match load()? {
        Some(mut l) if l.session.is_some() => {
            l.session = None;
            save(&l)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_in_real_keychain() {
        unsafe { std::env::set_var("ZTRITON_KEYCHAIN_SERVICE", "dev.ztriton.player.unittest") };
        delete().unwrap();
        assert_eq!(load().unwrap(), None);

        let login = SavedLogin {
            server: "http://example:8096".into(),
            username: "alice".into(),
            session: Some(SavedSession { user_id: "u1".into(), token: "t0k3n".into() }),
        };
        save(&login).unwrap();
        assert_eq!(load().unwrap(), Some(login.clone()));

        // Overwrite works (set_generic_password updates in place).
        let mut l2 = login.clone();
        l2.username = "bob".into();
        save(&l2).unwrap();
        assert_eq!(load().unwrap().unwrap().username, "bob");

        forget_session().unwrap();
        let kept = load().unwrap().unwrap();
        assert_eq!((kept.server.as_str(), kept.username.as_str(), kept.session), ("http://example:8096", "bob", None));

        delete().unwrap();
        delete().unwrap(); // idempotent
        assert_eq!(load().unwrap(), None);
        assert!(!format!("{login:?}").contains("t0k3n"), "Debug must not print the token");
    }
}
