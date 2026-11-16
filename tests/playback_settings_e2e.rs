//! End-to-end proof that the playback settings reach a real Jellyfin server in
//! one PlaybackInfo request and that the stream it returns fits them: direct
//! play when allowed, otherwise a transcode that mpv plays at the limited size,
//! restartable at the same spot with other settings.
//! Run: cargo test --release --test playback_settings_e2e -- --nocapture

use anyhow::{Context, Result, ensure};
use std::time::{Duration, Instant};
use ztriton::jellyfin::{Jellyfin, PlayMethod, PlaybackStream, redact};
use ztriton::playback_settings::{
    Bitrate, Compatibility, CustomFormats, MAXIMUM_BITRATE, MaxResolution, PlaybackSettings,
};
use ztriton::player::{Player, PlayerOptions};

#[path = "support/credentials.rs"]
mod credentials;

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

struct Log(Vec<String>);

impl Log {
    fn say(&mut self, line: String) {
        println!("{line}");
        self.0.push(line);
    }
}

fn describe(stream: &PlaybackStream) -> String {
    format!(
        "{}{} -> {}",
        stream.identity.method.as_str(),
        if stream.transcode_reasons.is_empty() { String::new() } else { format!(" [{}]", stream.transcode_reasons.join(",")) },
        redact(&stream.url)
    )
}

/// This device's session on the server, once its transcode is `max_height` tall or less
/// and taller than `min_height` (/Sessions does not echo the PlaySessionId).
fn transcoding_session(jf: &Jellyfin, min_height: i64, max_height: i64) -> Result<serde_json::Value> {
    let mut last = serde_json::Value::Null;
    for _ in 0..40 {
        let sessions = jf.sessions_json()?;
        if let Some(session) = sessions.as_array().into_iter().flatten().find(|session| {
            let height = session.pointer("/TranscodingInfo/Height").and_then(|h| h.as_i64()).unwrap_or(0);
            height > min_height && height <= max_height
        }) {
            return Ok(session.clone());
        }
        last = sessions.pointer("/0/TranscodingInfo").cloned().unwrap_or_default();
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("no transcode between {min_height}p and {max_height}p on the server; last TranscodingInfo: {last}")
}

fn describe_session(session: &serde_json::Value) -> String {
    let field = |pointer: &str| session.pointer(pointer).cloned().unwrap_or_default();
    format!(
        "PlayMethod={} TranscodingInfo={}x{} {} IsVideoDirect={} bitrate={} reasons={}",
        field("/PlayState/PlayMethod"),
        field("/TranscodingInfo/Width"),
        field("/TranscodingInfo/Height"),
        field("/TranscodingInfo/VideoCodec"),
        field("/TranscodingInfo/IsVideoDirect"),
        field("/TranscodingInfo/Bitrate"),
        field("/TranscodingInfo/TranscodeReasons"),
    )
}

#[test]
#[expect(unsafe_code, reason = "set_var gives the test its own device id")]
fn playback_settings_drive_server_transcoding() -> Result<()> {
    if let Err(e) = env_logger::builder().is_test(true).try_init() {
        eprintln!("logger already initialised: {e}");
    }
    let Some(credentials) = credentials::load()? else {
        eprintln!("SKIP: no Jellyfin server configured (set JELLYFIN_URL or fill in .env)");
        return Ok(());
    };
    // Separate device id so the test never invalidates the app's session.
    // SAFETY: set before this test starts any threads that read the environment.
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-e2e-playback-settings") };
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proof/playback-settings");
    std::fs::create_dir_all(&out)?;
    let mut log = Log(Vec::new());
    let result = run(&credentials, &mut log);
    if let Err(e) = &result {
        log.say(format!("FAILED: {e:#}"));
    }
    std::fs::write(out.join("e2e.log"), log.0.join("\n") + "\n")?;
    result
}

fn run(credentials: &credentials::Credentials, log: &mut Log) -> Result<()> {
    let jf = Jellyfin::login(&credentials.url, &credentials.user, &credentials.password)?;
    let search = std::env::var("ZTRITON_TEST_SEARCH").ok();
    let item = jf.first_video(search.as_deref())?;
    log.say(format!("jellyfin {}: picked {} '{}' ({})", jf.base, item.kind, item.name, item.id));

    // Auto bitrate: the speed test measures a real download speed.
    let measured = jf.measure_bitrate()?;
    log.say(format!("auto bitrate: speed test measured {:.1} Mbps", measured as f64 / 1e6));
    ensure!(measured > 0 && measured <= MAXIMUM_BITRATE, "measured {measured}");

    // Defaults: everything mpv plays natively goes as-is.
    let defaults = PlaybackSettings::default();
    let stream = jf.playback_info(&item.id, &defaults, measured, 0.0)?;
    log.say(format!("defaults (Max, Auto={measured}, Auto compat): {}", describe(&stream)));
    ensure!(stream.identity.method == PlayMethod::DirectPlay, "defaults should direct play");
    ensure!(stream.url.contains("static=true") && stream.url.contains("MediaSourceId="));

    // Direct Play claims everything; Maximum never limits.
    let direct = PlaybackSettings {
        compatibility: Compatibility::DirectPlay,
        bitrate: Bitrate::Limit(MAXIMUM_BITRATE),
        ..Default::default()
    };
    let stream = jf.playback_info(&item.id, &direct, MAXIMUM_BITRATE, 0.0)?;
    log.say(format!("Direct Play + Maximum: {}", describe(&stream)));
    ensure!(stream.identity.method == PlayMethod::DirectPlay);

    // Custom rules that exclude the file's container force a conversion.
    let custom = PlaybackSettings {
        compatibility: Compatibility::Custom,
        custom: CustomFormats { containers: "webm".into(), video_codecs: String::new(), audio_codecs: String::new() },
        ..Default::default()
    };
    let stream = jf.playback_info(&item.id, &custom, MAXIMUM_BITRATE, 0.0)?;
    log.say(format!("Custom (containers=webm): {}", describe(&stream)));
    ensure!(stream.identity.method == PlayMethod::Transcode);
    ensure!(stream.transcode_reasons.iter().any(|r| r == "ContainerNotSupported"), "{:?}", stream.transcode_reasons);

    // A bitrate below the file's forces a conversion.
    let low = PlaybackSettings { bitrate: Bitrate::Limit(100_000), ..Default::default() };
    let stream = jf.playback_info(&item.id, &low, 100_000, 0.0)?;
    log.say(format!("Bitrate 100 kbps: {}", describe(&stream)));
    ensure!(stream.identity.method == PlayMethod::Transcode);
    ensure!(stream.transcode_reasons.iter().any(|r| r.contains("Bitrate")), "{:?}", stream.transcode_reasons);

    // Resolution: a 360p limit makes the server downscale, and mpv receives the smaller picture.
    let small = PlaybackSettings { resolution: MaxResolution::Sd360, ..Default::default() };
    let stream = jf.playback_info(&item.id, &small, MAXIMUM_BITRATE, 0.0)?;
    log.say(format!("360p: {}", describe(&stream)));
    ensure!(stream.identity.method == PlayMethod::Transcode, "a video larger than 360p must be downscaled");
    ensure!(stream.transcode_reasons.iter().any(|r| r == "VideoResolutionNotSupported"));
    ensure!(!stream.url.to_ascii_lowercase().contains("apikey"), "the token must not be in the URL");

    let player = Player::new(PlayerOptions { no_audio: true, ..Default::default() })?;
    player.load(&stream.url, 0.0, &[jf.media_auth_header()])?;
    ensure!(
        wait_until(Duration::from_secs(60), || player.frames_rendered() > 0 && player.status().height > 0),
        "transcoded stream never rendered: {:?}",
        player.events()
    );
    let status = player.status();
    log.say(format!(
        "mpv plays the transcode: {}x{} {} (duration {:.1}s)",
        status.width, status.height, status.video_codec, status.duration
    ));
    ensure!(status.height <= 360 && status.width <= 640, "picture is {}x{}", status.width, status.height);
    ensure!(status.video_codec.to_ascii_lowercase().contains("h.264") || status.video_codec.contains("h264"));

    jf.report_start(&item.id, 0.0, &stream.identity)?;
    let play_session_id = stream.identity.play_session_id.clone().context("no PlaySessionId")?;
    let session = transcoding_session(&jf, 0, 360)?;
    log.say(format!("server /Sessions: {}", describe_session(&session)));
    ensure!(session.pointer("/PlayState/PlayMethod").and_then(|m| m.as_str()) == Some("Transcode"));

    // In-player change: restart at the same spot with a new resolution.
    ensure!(wait_until(Duration::from_secs(30), || player.status().position >= 3.0), "playback did not advance");
    let position = player.status().position;
    let larger = PlaybackSettings { resolution: MaxResolution::Sd480, ..small.clone() };
    let restarted = jf.playback_info(&item.id, &larger, MAXIMUM_BITRATE, position)?;
    log.say(format!("restart at {position:.2}s with 480p: {}", describe(&restarted)));
    let frames_before = player.frames_rendered();
    player.load(&restarted.url, position, &[jf.media_auth_header()])?;
    jf.stop_transcoding(&play_session_id)?;
    ensure!(
        wait_until(Duration::from_secs(60), || player.frames_rendered() > frames_before + 5
            && player.status().height > 360),
        "restarted stream never rendered at 480p: {:?}",
        player.status()
    );
    // Like the app: report once the new stream plays, so its transcode job exists.
    jf.report_progress(&item.id, player.status().position, false, &restarted.identity)?;
    let status = player.status();
    log.say(format!(
        "after restart mpv plays {}x{} at {:.2}s (was {position:.2}s)",
        status.width, status.height, status.position
    ));
    ensure!(status.height <= 480 && status.height > 360);
    ensure!((status.position - position).abs() < 15.0, "restart jumped from {position:.2}s to {:.2}s", status.position);
    let new_session_id = restarted.identity.play_session_id.clone().context("no PlaySessionId")?;
    ensure!(new_session_id != play_session_id, "a restart must be a new play session");
    let session = transcoding_session(&jf, 360, 480)?;
    log.say(format!("server /Sessions after restart: {}", describe_session(&session)));
    ensure!(session.pointer("/PlayState/PlayMethod").and_then(|m| m.as_str()) == Some("Transcode"));
    jf.report_stopped(&item.id, player.status().position, &restarted.identity)?;
    drop(player);

    // HDR / codec rules need an HDR HEVC video; the proof server has one.
    let hdr_search = std::env::var("ZTRITON_TEST_HDR_SEARCH").unwrap_or_else(|_| "HDR Proof".into());
    match jf.first_video(Some(&hdr_search)) {
        Ok(hdr) => {
            log.say(format!("HDR video: '{}' ({})", hdr.name, hdr.id));
            let stream = jf.playback_info(&hdr.id, &PlaybackSettings::default(), MAXIMUM_BITRATE, 0.0)?;
            log.say(format!("HDR, defaults: {}", describe(&stream)));
            ensure!(stream.identity.method == PlayMethod::DirectPlay);

            let force = PlaybackSettings { force_hdr_transcode: true, ..Default::default() };
            let stream = jf.playback_info(&hdr.id, &force, MAXIMUM_BITRATE, 0.0)?;
            log.say(format!("HDR, Force HDR/DV transcode: {}", describe(&stream)));
            ensure!(stream.identity.method == PlayMethod::Transcode);
            ensure!(stream.transcode_reasons.iter().any(|r| r == "VideoRangeTypeNotSupported"));
            ensure!(stream.url.contains("rangetype=SDR"), "the transcode must target SDR: {}", stream.url);

            let compatible = PlaybackSettings { compatibility: Compatibility::Compatible, ..Default::default() };
            let stream = jf.playback_info(&hdr.id, &compatible, MAXIMUM_BITRATE, 0.0)?;
            log.say(format!("HDR HEVC, Compatible: {}", describe(&stream)));
            ensure!(stream.identity.method == PlayMethod::Transcode);
            ensure!(stream.transcode_reasons.iter().any(|r| r.contains("CodecNotSupported")));

            let stream = jf.playback_info(&item.id, &compatible, MAXIMUM_BITRATE, 0.0)?;
            log.say(format!("'{}', Compatible: {}", item.name, describe(&stream)));
        }
        Err(e) => log.say(format!("SKIP HDR checks: no video matching '{hdr_search}' ({e:#})")),
    }
    log.say("OK".into());
    Ok(())
}
