//! Login -> save to Keychain -> restore from Keychain -> sign out, against a
//! live Jellyfin server. Uses a separate Keychain service and device id so it
//! never touches the app's own saved login or session.
//! Run: JELLYFIN_URL=.. JELLYFIN_USER=.. JELLYFIN_PASSWORD=.. cargo test --test auth_keychain_e2e -- --nocapture
#![expect(
    unused_crate_dependencies,
    reason = "integration tests see every dependency of the package"
)]

use anyhow::Context;
use std::process::Command;
use ztriton::jellyfin::Jellyfin;
use ztriton::keychain::{self, SavedLogin};

const SERVICE: &str = "dev.ztriton.player.e2etest";

/// Ask the `security` CLI (an independent reader) whether the item exists.
/// Errors if `security` can't be run, so a broken check never reads as "absent".
fn security_cli_has_item() -> std::io::Result<bool> {
    Ok(Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-a", "session"])
        .output()?
        .status
        .success())
}

#[test]
#[expect(unsafe_code, reason = "set_var isolates the test's Keychain item and device id")]
fn login_save_restore_signout() -> anyhow::Result<()> {
    let (Ok(url), Ok(user)) = (std::env::var("JELLYFIN_URL"), std::env::var("JELLYFIN_USER")) else {
        eprintln!("SKIP: JELLYFIN_URL / JELLYFIN_USER not set (needs a live Jellyfin server)");
        return Ok(());
    };
    let pw = std::env::var("JELLYFIN_PASSWORD").unwrap_or_default();
    // SAFETY: set before this test starts any threads that read the environment.
    unsafe { std::env::set_var("ZTRITON_KEYCHAIN_SERVICE", SERVICE) };
    // SAFETY: as above.
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-auth-e2e-test") };
    keychain::delete()?;
    assert!(!security_cli_has_item()?);

    // Wrong password is rejected with a readable error and nothing is saved.
    let err = Jellyfin::login(&url, &user, "definitely-wrong-password").expect_err("bad password must fail");
    println!("bad password -> {err}");
    assert!(err.to_string().contains("Incorrect username or password"));
    assert_eq!(keychain::load()?, None);

    // 1. Log in, save with "Remember me".
    let jf = Jellyfin::login(&url, &user, &pw).expect("login");
    println!("login ok: user_id={}", jf.user_id);
    keychain::save(&SavedLogin { server: jf.base.clone(), username: user.clone(), session: Some(jf.saved_session()) })
        ?;
    assert!(security_cli_has_item()?, "`security` CLI must see the Keychain item");
    let raw = Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-a", "session", "-w"])
        .output()
        ?;
    let raw = String::from_utf8_lossy(&raw.stdout);
    if !pw.is_empty() {
        assert!(!raw.contains(&pw), "plaintext password must not be stored");
    }
    println!("keychain item fields: {:?}", serde_json::from_str::<serde_json::Value>(&raw)?.as_object().context("Keychain item is not a JSON object")?.keys().collect::<Vec<_>>());

    // 2. "Relaunch": build a fresh client purely from the Keychain.
    let saved = keychain::load()?.expect("saved login");
    let s = saved.session.clone().expect("token saved");
    let restored = Jellyfin::resume_session(&saved.server, &s.user_id, &s.token).expect("resume from keychain");
    assert_eq!(restored.user_id, jf.user_id);
    let views = restored.views().expect("restored session can read library");
    println!("restored session ok: {} libraries", views.len());

    // 3. Sign out: revoke server-side + forget token, keep server/username.
    restored.logout().expect("logout");
    keychain::forget_session()?;
    let after = keychain::load()?.expect("server/username kept for prefill");
    assert_eq!(after.session, None);
    assert_eq!(after.username, user);
    let e = Jellyfin::resume_session(&saved.server, &s.user_id, &s.token).expect_err("revoked token must fail");
    println!("old token after sign-out -> {e}");
    assert!(e.to_string().contains("401"), "expected HTTP 401, got: {e}");

    keychain::delete()?;
    assert!(!security_cli_has_item()?);
    println!("PASS");
    Ok(())
}
