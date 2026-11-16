//! Proof that a lower bitrate keeps playback smooth on a slow connection.
//!
//! A local proxy caps the link from the Jellyfin server to the player (like
//! slow Wi-Fi), then the same high-bitrate movie plays through ztriton's
//! player three times: at Maximum (sent as-is), at a fixed bitrate below the
//! link, and at Auto (whose speed test runs through the same slow link). Each
//! run counts stalls ("Buffering…") and how much of the movie played.
//!
//! Run: cargo test --release --test slow_network_e2e -- --nocapture
//! Needs a server video whose bitrate is well above the cap
//! (ZTRITON_TEST_SLOW_SEARCH, default "Bitrate Proof"); the link speed is
//! ZTRITON_SLOW_LINK_KBPS (default 4000).

use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ztriton::jellyfin::{Jellyfin, PlayMethod, PlaybackStream};
use ztriton::playback_settings::{Bitrate, MAXIMUM_BITRATE, PlaybackSettings, format_bitrate};
use ztriton::player::{Player, PlayerOptions};

#[path = "support/credentials.rs"]
mod credentials;

/// One shared budget for every connection, like a single slow link.
struct Link {
    bytes_per_second: f64,
    next_free: Mutex<Instant>,
}

impl Link {
    fn wait_to_send(&self, bytes: usize) {
        let duration = Duration::from_secs_f64(bytes as f64 / self.bytes_per_second);
        let send_at = {
            let mut next_free = self.next_free.lock();
            let start = (*next_free).max(Instant::now());
            *next_free = start + duration;
            start + duration
        };
        if let Some(wait) = send_at.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream, link: Option<Arc<Link>>) {
    let mut buffer = [0u8; 8192];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if let Some(link) = &link {
            link.wait_to_send(read);
        }
        if to.write_all(&buffer[..read]).is_err() {
            break;
        }
    }
    if let Err(e) = to.shutdown(std::net::Shutdown::Write) {
        log::debug!("proxy shutdown: {e}");
    }
}

/// Forward 127.0.0.1:<port> to `upstream`, capping server-to-player bytes at `kilobits_per_second`.
fn start_slow_proxy(upstream: String, kilobits_per_second: u64) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let link = Arc::new(Link {
        bytes_per_second: kilobits_per_second as f64 * 1000.0 / 8.0,
        next_free: Mutex::new(Instant::now()),
    });
    // Detached: the proxy lives as long as the test process.
    drop(std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(&upstream) else { continue };
            let (Ok(client_reader), Ok(server_reader)) = (client.try_clone(), server.try_clone()) else { continue };
            let link = link.clone();
            drop(std::thread::spawn(move || pipe(client_reader, server, None)));
            drop(std::thread::spawn(move || pipe(server_reader, client, Some(link))));
        }
    }));
    Ok(port)
}

struct Watched {
    startup: f64,
    stalls: u32,
    stalled_seconds: f64,
    movie_seconds: f64,
}

/// Play `stream` for `wall` and measure how smoothly it went.
fn watch(jf: &Jellyfin, stream: &PlaybackStream, wall: Duration) -> Result<Watched> {
    let player = Player::new(PlayerOptions { no_audio: true, ..Default::default() })?;
    let started = Instant::now();
    player.load(&stream.url, 0.0, &[jf.media_auth_header()])?;
    let mut first_frame = None;
    let mut start_position = 0.0;
    let mut stalls = 0;
    let mut stalled_seconds = 0.0;
    let mut was_buffering = false;
    let tick = Duration::from_millis(100);
    while started.elapsed() < wall {
        std::thread::sleep(tick);
        let status = player.status();
        if first_frame.is_none() {
            if player.frames_of_current_file() > 0 {
                first_frame = Some(started.elapsed().as_secs_f64());
                start_position = status.position;
            }
            continue;
        }
        if status.buffering {
            stalled_seconds += tick.as_secs_f64();
            if !was_buffering {
                stalls += 1;
            }
        }
        was_buffering = status.buffering;
    }
    let movie_seconds = player.status().position - start_position;
    let startup = first_frame.context("no frame was ever shown")?;
    if let Some(play_session_id) = stream.identity.play_session_id.as_deref()
        && stream.identity.method == PlayMethod::Transcode
    {
        jf.stop_transcoding(play_session_id)?;
    }
    Ok(Watched { startup, stalls, stalled_seconds, movie_seconds })
}

#[test]
#[expect(unsafe_code, reason = "set_var gives the test its own device id")]
fn lower_bitrate_plays_smoothly_on_a_slow_link() -> Result<()> {
    if let Err(e) = env_logger::builder().is_test(true).try_init() {
        eprintln!("logger already initialised: {e}");
    }
    let Some(credentials) = credentials::load()? else {
        eprintln!("SKIP: no Jellyfin server configured (set JELLYFIN_URL or fill in .env)");
        return Ok(());
    };
    // SAFETY: set before this test starts any threads that read the environment.
    unsafe { std::env::set_var("ZTRITON_DEVICE_ID", "ztriton-e2e-slow-network") };
    let link_kbps: u64 = std::env::var("ZTRITON_SLOW_LINK_KBPS").ok().and_then(|v| v.parse().ok()).unwrap_or(4000);
    let wall = Duration::from_secs(40);
    let search = std::env::var("ZTRITON_TEST_SLOW_SEARCH").unwrap_or_else(|_| "Bitrate Proof".into());

    let direct = Jellyfin::login(&credentials.url, &credentials.user, &credentials.password)?;
    let Ok(item) = direct.first_video(Some(&search)) else {
        eprintln!("SKIP: no video matching '{search}' on the server");
        return Ok(());
    };
    let upstream = url::Url::parse(&direct.base)?;
    let upstream = format!(
        "{}:{}",
        upstream.host_str().context("server URL has no host")?,
        upstream.port_or_known_default().context("server URL has no port")?
    );
    let port = start_slow_proxy(upstream, link_kbps)?;
    let jf = Jellyfin::login(&format!("http://127.0.0.1:{port}"), &credentials.user, &credentials.password)?;

    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proof/slow-network");
    std::fs::create_dir_all(&out)?;
    let mut log = Vec::<String>::new();
    let mut say = |line: String| {
        println!("{line}");
        log.push(line);
    };
    say(format!("link capped at {} between server and player; movie '{}'; {}s of wall time per run", format_bitrate(link_kbps * 1000), item.name, wall.as_secs()));

    let lowered_limit = link_kbps * 1000 / 2;
    let measured = jf.measure_bitrate()?;
    say(format!("Auto speed test through the slow link measured {}", format_bitrate(measured)));
    let runs = [
        ("Maximum", Bitrate::Limit(MAXIMUM_BITRATE), MAXIMUM_BITRATE),
        ("Lowered", Bitrate::Limit(lowered_limit), lowered_limit),
        ("Auto", Bitrate::Auto, measured),
    ];
    let mut results = Vec::new();
    for (name, bitrate, max_bitrate) in runs {
        let settings = PlaybackSettings { bitrate, ..Default::default() };
        let stream = jf.playback_info(&item.id, &settings, max_bitrate, 0.0)?;
        let watched = watch(&jf, &stream, wall)?;
        let played_after_start = wall.as_secs_f64() - watched.startup;
        say(format!(
            "{name:8} bitrate {:>9}: {:<11}{} | first frame after {:.1}s | {} stall(s), {:.1}s stalled | {:.1}s of movie in {:.1}s ({:.0}% of real time)",
            settings.bitrate.label(),
            stream.identity.method.as_str(),
            if stream.transcode_reasons.is_empty() { String::new() } else { format!(" [{}]", stream.transcode_reasons.join(",")) },
            watched.startup,
            watched.stalls,
            watched.stalled_seconds,
            watched.movie_seconds,
            played_after_start,
            100.0 * watched.movie_seconds / played_after_start
        ));
        results.push((name, stream.identity.method, watched));
    }
    std::fs::write(out.join("e2e.log"), log.join("\n") + "\n")?;

    let [(_, maximum_method, maximum), (_, lowered_method, lowered), (_, _, auto)] = results.as_slice() else {
        anyhow::bail!("expected three runs");
    };
    ensure!(*maximum_method == PlayMethod::DirectPlay, "Maximum should send the file as-is");
    ensure!(*lowered_method == PlayMethod::Transcode, "a bitrate below the file's must transcode");
    ensure!(maximum.stalls > 0, "the file as-is should not fit through the slow link");
    ensure!(lowered.stalled_seconds < maximum.stalled_seconds / 4.0, "lowering the bitrate should remove most stalling");
    ensure!(lowered.movie_seconds > maximum.movie_seconds * 1.5, "lowering the bitrate should play far more of the movie");
    ensure!(auto.movie_seconds > maximum.movie_seconds, "Auto should beat Maximum on a slow link");
    Ok(())
}
