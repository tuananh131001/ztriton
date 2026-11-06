//! External (sidecar) subtitles from a live Jellyfin server reach mpv, can be
//! chosen, and a remembered choice is re-applied to the next episode.
//! Uses the app's saved Keychain session (read-only) or JELLYFIN_URL/USER/PASSWORD.
//! Run: cargo test --release --test subtitles_e2e -- --nocapture

use anyhow::{Context, Result, anyhow, ensure};
use std::time::{Duration, Instant};
use ztriton::jellyfin::Jellyfin;
use ztriton::player::{Player, PlayerOptions, SubtitlePreference, Track};

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn connect() -> Result<Option<Jellyfin>> {
    if let Ok(url) = std::env::var("JELLYFIN_URL") {
        let user = std::env::var("JELLYFIN_USER").context("JELLYFIN_USER")?;
        let password = std::env::var("JELLYFIN_PASSWORD").unwrap_or_default();
        return Ok(Some(Jellyfin::login(&url, &user, &password)?));
    }
    let Some(login) = ztriton::keychain::load()? else { return Ok(None) };
    let Some(session) = login.session else { return Ok(None) };
    Ok(Some(Jellyfin::resume_session(&login.server, &session.user_id, &session.token)?))
}

fn subtitles(player: &Player) -> Vec<Track> {
    player.tracks().into_iter().filter(|t| t.kind == "sub").collect()
}

fn selected_subtitle(player: &Player) -> Option<Track> {
    subtitles(player).into_iter().find(|t| t.selected)
}

/// Wait for mpv to be showing a subtitle cue, then pause so the frame holds it.
fn pause_on_cue(player: &Player) -> Result<String> {
    player.set_paused(false);
    ensure!(
        wait_until(Duration::from_secs(30), || !player.property_string("sub-text").is_empty()),
        "no subtitle cue appeared"
    );
    player.set_paused(true);
    std::thread::sleep(Duration::from_millis(600));
    Ok(player.property_string("sub-text"))
}

fn open(jf: &Jellyfin, search: &str, preference: Option<SubtitlePreference>, say: &mut impl FnMut(String)) -> Result<Player> {
    let item = jf.first_video(Some(search))?;
    say(format!("item: {} · {} ({})", item.subtitle(), item.name, item.id));
    let external = jf.external_subtitles(&item.id)?;
    for subtitle in &external {
        say(format!("  jellyfin external subtitle: [{}] {}", subtitle.language, subtitle.title));
    }
    let player = Player::new(PlayerOptions { no_audio: true, ..Default::default() })?;
    player.load_with_subtitles(&jf.stream_url(&item.id), 0.0, &[jf.media_auth_header()], external, preference)?;
    ensure!(wait_until(Duration::from_secs(30), || subtitles(&player).len() >= 2), "mpv never saw the subtitle tracks");
    for track in subtitles(&player) {
        say(format!("  mpv sub track id={} lang={} selected={} label={:?}", track.id, track.language, track.selected, track.label));
    }
    Ok(player)
}

#[test]
fn external_subtitles_choose_and_remember() -> Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-e2e-test") };
    let Some(jf) = connect()? else {
        eprintln!("SKIP: no Jellyfin session (sign in to the app or set JELLYFIN_URL)");
        return Ok(());
    };
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proof/subtitles");
    std::fs::create_dir_all(&out)?;
    let mut log = Vec::<String>::new();
    let mut say = |line: String| {
        println!("{line}");
        log.push(line);
    };

    // 1. Fizbo (S01E09): two sidecar .srt files, none embedded in the mkv.
    say("== Modern Family S01E09, no remembered choice ==".into());
    let player = open(&jf, "Fizbo", None, &mut say)?;
    let vietnamese = subtitles(&player)
        .into_iter()
        .find(|t| t.language == "vie")
        .ok_or_else(|| anyhow!("no Vietnamese track"))?;
    let english = subtitles(&player)
        .into_iter()
        .find(|t| t.language == "eng")
        .ok_or_else(|| anyhow!("no English track"))?;

    player.select_subtitle(Some(vietnamese.id))?;
    let text = pause_on_cue(&player)?;
    say(format!("selected Vietnamese -> mpv sub-text at {:.2}s: {text:?}", player.status().position));
    ensure!(selected_subtitle(&player).map(|t| t.id) == Some(vietnamese.id), "Vietnamese not selected");
    let with_subtitle = player.latest_frame().ok_or_else(|| anyhow!("no frame"))?;
    with_subtitle.save_png(&out.join("subtitle_vie_frame.png"))?;

    player.select_subtitle(None)?;
    std::thread::sleep(Duration::from_millis(600));
    let off_text = player.property_string("sub-text");
    say(format!("selected Off -> mpv sub-text: {off_text:?}"));
    ensure!(off_text.is_empty() && selected_subtitle(&player).is_none(), "subtitles not off");
    let without_subtitle = player.latest_frame().ok_or_else(|| anyhow!("no frame"))?;
    without_subtitle.save_png(&out.join("subtitle_off_frame.png"))?;
    ensure!(with_subtitle.fingerprint() != without_subtitle.fingerprint(), "frame unchanged when subtitles turned off");

    player.select_subtitle(Some(english.id))?;
    let english_text = pause_on_cue(&player)?;
    say(format!("selected English -> mpv sub-text at {:.2}s: {english_text:?}", player.status().position));
    ensure!(english_text != text, "English cue identical to Vietnamese cue");

    // The UI stores exactly this when the user picks a track.
    let remembered = SubtitlePreference::from_track(Some(&vietnamese));
    say(format!("remembered preference: {remembered:?}"));
    drop(player);

    // 2. Next episode in a fresh player: the preference alone picks Vietnamese.
    say("== Modern Family S01E10, remembered choice applied ==".into());
    let player = open(&jf, "Undeck the Halls", Some(remembered), &mut say)?;
    ensure!(
        wait_until(Duration::from_secs(10), || selected_subtitle(&player).is_some_and(|t| t.language == "vie")),
        "remembered Vietnamese choice not applied"
    );
    let text = pause_on_cue(&player)?;
    say(format!("auto-selected {:?} -> mpv sub-text at {:.2}s: {text:?}", selected_subtitle(&player).map(|t| t.label), player.status().position));
    player.latest_frame().ok_or_else(|| anyhow!("no frame"))?.save_png(&out.join("subtitle_remembered_frame.png"))?;

    say("--- mpv events ---".into());
    for event in player.events() {
        say(event);
    }
    std::fs::write(out.join("subtitles_e2e.log"), log.join("\n") + "\n")?;
    Ok(())
}
