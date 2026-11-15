//! Library search against a live Jellyfin server: results come back grouped
//! into movies, shows and episodes, and a nonsense term finds nothing.
//! Uses JELLYFIN_URL/USER/PASSWORD (environment or .env), else the app's saved Keychain session (read-only).
//! Run: cargo test --release --test search_e2e -- --nocapture

use anyhow::{Result, ensure};
use ztriton::jellyfin::Jellyfin;

#[path = "support/credentials.rs"]
mod credentials;

fn connect() -> Result<Option<Jellyfin>> {
    if let Some(credentials) = credentials::load()? {
        return Ok(Some(Jellyfin::login(&credentials.url, &credentials.user, &credentials.password)?));
    }
    let Some(login) = ztriton::keychain::load()? else { return Ok(None) };
    let Some(session) = login.session else { return Ok(None) };
    Ok(Some(Jellyfin::resume_session(&login.server, &session.user_id, &session.token)?))
}

#[test]
#[expect(unsafe_code, reason = "set_var gives the test its own device id")]
fn search_groups_results_by_kind() -> Result<()> {
    if let Err(e) = env_logger::builder().is_test(true).try_init() {
        eprintln!("logger already initialised: {e}");
    }
    // Separate device id so the test never invalidates the app's session.
    // SAFETY: set before this test starts any threads that read the environment.
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-e2e-test") };
    let Some(jf) = connect()? else {
        eprintln!("SKIP: no Jellyfin session (sign in to the app or set JELLYFIN_URL)");
        return Ok(());
    };

    let video = jf.first_video(None)?;
    println!("searching for {} '{}' ({})", video.kind, video.name, video.id);
    let results = jf.search(&video.name, 100)?;
    println!(
        "results: {} movies, {} shows, {} episodes",
        results.movies.len(),
        results.shows.len(),
        results.episodes.len()
    );
    ensure!(results.movies.iter().all(|item| item.kind == "Movie"), "a non-movie is grouped as a movie");
    ensure!(results.shows.iter().all(|item| item.kind == "Series"), "a non-series is grouped as a show");
    ensure!(results.episodes.iter().all(|item| item.kind == "Episode"), "a non-episode is grouped as an episode");
    let group = if video.kind == "Movie" { &results.movies } else { &results.episodes };
    ensure!(
        group.iter().any(|item| item.id == video.id),
        "searching for '{}' did not find {}",
        video.name,
        video.id
    );

    let nothing = jf.search("zzqxj-no-such-title-zzqxj", 100)?;
    ensure!(nothing.is_empty(), "nonsense term found {} results", nothing.len());
    Ok(())
}
