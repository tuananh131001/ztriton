//! End-to-end proof: real Jellyfin server -> libmpv (mpv 0.42) -> SW render frames.
//! Run: cargo test --release --test jellyfin_e2e -- --nocapture

use std::time::{Duration, Instant};
use ztriton::jellyfin::{Jellyfin, redact};
use ztriton::player::{Player, PlayerOptions, client_api_version};

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn jellyfin_stream_plays_through_libmpv() {
    let _ = env_logger::builder().is_test(true).try_init();
    if std::env::var("JELLYFIN_URL").is_err() {
        eprintln!("SKIP: JELLYFIN_URL not set (needs a live Jellyfin server)");
        return;
    }
    // Separate device id so the test never invalidates the app's session.
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-e2e-test") };
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proof/playback");
    std::fs::create_dir_all(&out).unwrap();
    let mut log = Vec::<String>::new();
    macro_rules! say { ($($t:tt)*) => {{ let s = format!($($t)*); println!("{s}"); log.push(s); }} }

    // 1. libmpv version
    let (maj, min) = client_api_version();
    say!("libmpv client API: {maj}.{min}");
    assert!(maj == 2 && min >= 6, "expected mpv 0.42 (client API 2.6+)");

    // 2. Jellyfin auth + library
    let jf = Jellyfin::login(
        &std::env::var("JELLYFIN_URL").unwrap(),
        &std::env::var("JELLYFIN_USER").expect("JELLYFIN_USER"),
        &std::env::var("JELLYFIN_PASSWORD").unwrap_or_default(),
    )
    .expect("login");
    say!("jellyfin: logged in, user_id={}", jf.user_id);
    let views = jf.views().unwrap();
    say!("jellyfin: views = {:?}", views.iter().map(|v| &v.name).collect::<Vec<_>>());
    assert!(!views.is_empty());
    let item = jf.first_video(std::env::var("ZTRITON_TEST_SEARCH").ok().as_deref()).unwrap();
    say!("jellyfin: picked {} '{}' ({}), runtime {:?}s", item.kind, item.name, item.id, item.runtime_secs());
    let url = jf.stream_url(&item.id);
    say!("stream url: {}", redact(&url));

    // 3. Play through libmpv, headless, no audio device
    assert!(!url.to_ascii_lowercase().contains("api_key") && !url.contains("Token"), "token must not be in the URL");
    let player = Player::new(PlayerOptions { no_audio: true, ..Default::default() }).unwrap();
    let version = player.property_string("mpv-version");
    say!("mpv-version property: {version}");
    assert!(version.starts_with("mpv v0.42"), "loaded mpv is {version}");
    player.load(&url, 0.0, &[jf.media_auth_header()]).unwrap();
    assert_eq!(player.property_string("http-header-fields"), "", "auth header must not be set globally");

    assert!(wait_until(Duration::from_secs(30), || player.status().duration > 0.0), "duration never became known");
    let st = player.status();
    say!("loaded: duration={:.1}s codec={} {}x{} hwdec={}", st.duration, st.video_codec, st.width, st.height, st.hwdec);

    assert!(wait_until(Duration::from_secs(20), || player.frames_rendered() > 0), "no frame rendered");
    let t0 = Instant::now();
    let p0 = player.status().position;
    let f0 = player.frames_rendered();
    std::thread::sleep(Duration::from_secs(6));
    let p1 = player.status().position;
    let f1 = player.frames_rendered();
    let wall = t0.elapsed().as_secs_f64();
    let fps = (f1 - f0) as f64 / wall;
    say!("playback: time-pos {p0:.2}s -> {p1:.2}s over {wall:.2}s wall; frames {f0} -> {f1} ({fps:.1} fps)");
    assert!(p1 - p0 >= 3.0, "time-pos did not advance enough");
    assert!(f1 - f0 >= 30, "too few frames rendered");

    let frame = player.latest_frame().unwrap();
    let var = frame.luma_variance();
    say!("frame #{}: {}x{} luma variance {var:.1}", frame.seq, frame.width, frame.height);
    frame.save_png(&out.join("frame_test_start.png")).unwrap();

    // 4. Seek to 50%
    let target = st.duration * 0.5;
    player.seek_absolute(target);
    assert!(wait_until(Duration::from_secs(20), || (player.status().position - target).abs() < 5.0), "seek failed");
    let before = player.frames_rendered();
    assert!(wait_until(Duration::from_secs(10), || player.frames_rendered() > before + 10));
    std::thread::sleep(Duration::from_millis(1500));
    let pos = player.status().position;
    say!("seek: target {target:.1}s -> time-pos {pos:.1}s");
    let frame2 = player.latest_frame().unwrap();
    let var2 = frame2.luma_variance();
    say!("frame #{} after seek: luma variance {var2:.1}", frame2.seq);
    frame2.save_png(&out.join("frame_test_mid.png")).unwrap();
    assert!(var.max(var2) > 50.0, "frames look blank");
    assert_ne!(frame.fingerprint(), frame2.fingerprint(), "frames identical across seek");

    say!("--- mpv events ---");
    for e in player.events() { say!("{e}"); }
    std::fs::write(out.join("test_e2e.log"), log.join("\n") + "\n").unwrap();
}
