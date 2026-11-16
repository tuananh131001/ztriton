//! Full-window video player: mpv frames as NV12 IOSurfaces drawn by GPUI's
//! Metal `surface()` element, with an auto-hiding Infuse-style control overlay.

use super::*;
use futures::StreamExt;
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, ObjectFit, Render, SharedString, Window, actions, div,
    linear_color_stop, linear_gradient, prelude::*, px, rgba,
};
use std::time::{Duration, Instant};
use ztriton::jellyfin::{Item, Jellyfin, PlayMethod, PlaybackStream, StreamIdentity, adjacent_episodes, redact};
use ztriton::playback_settings::{Bitrate, MAXIMUM_BITRATE, MaxResolution, PlaybackSettings, format_bitrate};
use ztriton::player::{Player, PlayerOptions, Status, SubtitlePreference, VideoFrame};
use ztriton::preferences::Preferences;

actions!(
    player,
    [TogglePause, SeekForward, SeekBack, SeekForwardBig, SeekBackBig, VolumeUp, VolumeDown, CycleAudio, CycleSubs, ClosePlayer, Fullscreen, ToggleMute, PreviousEpisode, NextEpisode]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Player");
    cx.bind_keys([
        KeyBinding::new("space", TogglePause, ctx),
        KeyBinding::new("right", SeekForward, ctx),
        KeyBinding::new("left", SeekBack, ctx),
        KeyBinding::new("shift-right", SeekForwardBig, ctx),
        KeyBinding::new("shift-left", SeekBackBig, ctx),
        KeyBinding::new("up", VolumeUp, ctx),
        KeyBinding::new("down", VolumeDown, ctx),
        KeyBinding::new("a", CycleAudio, ctx),
        KeyBinding::new("s", CycleSubs, ctx),
        KeyBinding::new("escape", ClosePlayer, ctx),
        KeyBinding::new("f", Fullscreen, ctx),
        KeyBinding::new("m", ToggleMute, ctx),
        KeyBinding::new("p", PreviousEpisode, ctx),
        KeyBinding::new("n", NextEpisode, ctx),
    ]);
}

pub struct Closed;

/// Playback of this item stopped so `.0` can be played instead.
pub struct PlayEpisode(pub Item);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Subtitles,
    Quality,
}

/// The stream mpv is playing and the settings it was requested with.
struct ActiveStream {
    identity: StreamIdentity,
    transcode_reasons: Vec<String>,
    max_bitrate: u64,
}

/// A report owed to the server once the new stream shows its first frame:
/// Jellyfin records a transcode report as direct play if it arrives before
/// mpv has requested the stream and started the transcode job.
#[derive(Clone, Copy)]
enum PendingReport {
    Start,
    Restart,
}

/// Everything worked out off the UI thread before a stream can be loaded.
struct OpenedStream {
    settings: PlaybackSettings,
    subtitle: Option<SubtitlePreference>,
    max_bitrate: u64,
    speed_test_error: Option<anyhow::Error>,
    stream: anyhow::Result<PlaybackStream>,
}

pub struct PlayerView {
    player: Option<Player>,
    jf: Jellyfin,
    item: Item,
    /// Where playback was asked to start, in seconds.
    start: f64,
    focus: FocusHandle,
    frame: Option<Arc<VideoFrame>>,
    status: Status,
    tracks: Vec<ztriton::player::Track>,
    last_activity: Instant,
    error: Option<String>,
    osd: Option<(String, Instant)>,
    open_menu: Option<Menu>,
    /// This playback's settings: the saved defaults until changed from the
    /// Quality menu, which never changes the saved ones. `None` until loaded.
    settings: Option<PlaybackSettings>,
    stream: Option<ActiveStream>,
    pending_report: Option<PendingReport>,
    /// Asks the server for a stream; replacing it cancels an older request.
    stream_task: Option<gpui::Task<()>>,
    previous_episode: Option<Item>,
    next_episode: Option<Item>,
    // Stats (shown in proof mode / logged)
    presented: u64,
    presented_window: (Instant, u64),
    presented_fps: f64,
    closed: bool,
    _quit_hook: Option<gpui::Subscription>,
}

impl EventEmitter<Closed> for PlayerView {}
impl EventEmitter<PlayEpisode> for PlayerView {}

const BAR_MARGIN: f32 = 40.;

impl PlayerView {
    pub fn new(jf: Jellyfin, item: Item, start: f64, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut this = Self {
            player: None,
            jf: jf.clone(),
            item: item.clone(),
            start,
            focus,
            frame: None,
            status: Status::default(),
            tracks: vec![],
            last_activity: Instant::now(),
            error: None,
            osd: None,
            open_menu: None,
            settings: None,
            stream: None,
            pending_report: None,
            stream_task: None,
            previous_episode: None,
            next_episode: None,
            presented: 0,
            presented_window: (Instant::now(), 0),
            presented_fps: 0.0,
            closed: false,
            _quit_hook: None,
        };

        match Player::new(PlayerOptions {
            // Automated/proof runs never make noise; ZTRITON_MUTE=1 for normal runs too.
            muted: crate::proof_mode() || std::env::var("ZTRITON_MUTE").is_ok_and(|v| v == "1"),
            ..Default::default()
        }) {
            Ok(player) => {
                // Render thread -> UI wakeup.
                let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
                player.set_frame_callback(move || {
                    // Fails only once the view is gone and the player is shutting down.
                    drop(tx.unbounded_send(()));
                });
                proof_log(format!(
                    "player: libmpv {} | asking the server how to stream '{}' (start {:.1}s)",
                    player.property_string("mpv-version"),
                    item.name,
                    start
                ));
                this.player = Some(player);
                this.open_stream(start, None, None, cx);

                cx.spawn(async move |this, cx| {
                    while rx.next().await.is_some() {
                        // Coalesce bursts: drain whatever else is queued.
                        while let Ok(()) = rx.try_recv() {}
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                })
                .detach();

                if item.kind == "Episode"
                    && let Some(series_id) = item.series_id.clone()
                {
                    Self::load_adjacent_episodes(jf.clone(), series_id, item.id.clone(), cx);
                }

                // Jellyfin progress reporting; the start is reported once a stream is chosen.
                cx.spawn(async move |this, cx| {
                    let mut tick: u64 = 0;
                    loop {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                        tick += 1;
                        let Ok(Some((st, stream, report_now))) = this.update(cx, |v, cx| {
                            let player = v.player.as_ref()?;
                            let position = playing_position(player, &player.status());
                            let report_now = position.and_then(|position| Some((v.pending_report.take()?, position)));
                            let st = v.refresh_status();
                            // Saved on the UI thread so saves land in order, and the last one before quit.
                            if tick.is_multiple_of(10)
                                && let Some(position) = position
                            {
                                v.save_position(position, st.duration);
                            }
                            cx.notify();
                            // No progress for a stream that has not started: it would arrive before the start.
                            let stream = v.stream.as_ref().filter(|_| v.pending_report.is_none()).map(|stream| stream.identity.clone());
                            Some((st, stream, report_now))
                        }) else {
                            break;
                        };
                        if let Some((report, position)) = report_now
                            && let Some(stream) = stream.clone()
                        {
                            let jf = jf.clone();
                            let id = item.id.clone();
                            cx.background_executor()
                                .spawn(async move {
                                    // A restart moves the server's session to the new stream.
                                    let reported = match report {
                                        PendingReport::Start => jf.report_start(&id, position, &stream),
                                        PendingReport::Restart => jf.report_progress(&id, position, false, &stream),
                                    };
                                    match reported {
                                        Ok(()) => proof_log(format!("player: reported {} of the {} stream at {position:.2}s", match report {
                                            PendingReport::Start => "the start",
                                            PendingReport::Restart => "the restart",
                                        }, stream.method.as_str())),
                                        Err(e) => log::warn!("playback report: {e:#}"),
                                    }
                                })
                                .detach();
                        } else if tick.is_multiple_of(10)
                            && st.duration > 0.0
                            && let Some(stream) = stream
                        {
                            let jf = jf.clone();
                            let id = item.id.clone();
                            cx.background_executor()
                                .spawn(async move {
                                    if let Err(e) = jf.report_progress(&id, st.position, st.paused, &stream) {
                                        log::warn!("report_progress: {e}");
                                    }
                                })
                                .detach();
                        }
                    }
                })
                .detach();

                // Cmd-Q / last window closed: stop mpv and report to Jellyfin.
                this._quit_hook = Some(cx.on_app_quit(|v, cx| {
                    let task = v.shutdown(cx);
                    async move {
                        if let Some(t) = task {
                            t.await;
                        }
                    }
                }));

                if crate::proof_mode() {
                    Self::spawn_proof_script(cx);
                }
            }
            Err(e) => this.error = Some(format!("{e:#}")),
        }
        this
    }

    /// Ask the server for a stream that fits the playback settings, then play
    /// it from `start`. `change` is a Quality menu choice for this session;
    /// `None` uses the saved settings and subtitle choice.
    /// `measured_bitrate` is an earlier Auto speed test to reuse: a new test
    /// would compete with the stream still downloading and under-measure.
    fn open_stream(&mut self, start: f64, change: Option<PlaybackSettings>, measured_bitrate: Option<u64>, cx: &mut Context<Self>) {
        let jf = self.jf.clone();
        let item_id = self.item.id.clone();
        // A restart keeps the subtitle on screen rather than the saved one.
        let shown_subtitle = change.as_ref().map(|_| {
            SubtitlePreference::from_track(self.tracks.iter().find(|t| t.kind == "sub" && t.selected))
        });
        self.stream_task = Some(cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    let (settings, subtitle) = match change {
                        Some(settings) => (settings, shown_subtitle),
                        None => {
                            let preferences = Preferences::load().unwrap_or_else(|e| {
                                log::warn!("preferences: {e:#}");
                                Preferences::default()
                            });
                            (preferences.playback, preferences.subtitle)
                        }
                    };
                    let (max_bitrate, speed_test_error) = match (settings.bitrate, measured_bitrate) {
                        (Bitrate::Limit(bits_per_second), _) => (bits_per_second, None),
                        (Bitrate::Auto, Some(bits_per_second)) => (bits_per_second, None),
                        (Bitrate::Auto, None) => match jf.measure_bitrate() {
                            Ok(bits_per_second) => {
                                proof_log(format!("bitrate: speed test measured {}", format_bitrate(bits_per_second)));
                                (bits_per_second, None)
                            }
                            Err(e) => (MAXIMUM_BITRATE, Some(e)),
                        },
                    };
                    let stream = jf.playback_info(&item_id, &settings, max_bitrate, start);
                    OpenedStream { settings, subtitle, max_bitrate, speed_test_error, stream }
                })
                .await;
            this.update(cx, |view, cx| view.play_stream(opened, start, cx)).log_err();
        }));
    }

    fn play_stream(&mut self, opened: OpenedStream, start: f64, cx: &mut Context<Self>) {
        let OpenedStream { settings, subtitle, max_bitrate, speed_test_error, stream } = opened;
        let restarting = self.stream.is_some();
        if let Some(e) = speed_test_error {
            log::warn!("speed test: {e:#}");
            proof_log(format!("bitrate: speed test failed ({e:#}), using Maximum"));
            self.flash(format!("Speed test failed, using Maximum: {e}"), cx);
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                proof_log(format!("player: the server chose no stream: {e:#}"));
                if restarting {
                    // The previous stream keeps playing.
                    self.flash(format!("Couldn't change quality: {e}"), cx);
                } else {
                    self.error = Some(format!("{e:#}"));
                    cx.notify();
                }
                return;
            }
        };
        proof_log(format!(
            "player: server chose {}{} for {} · {} · {}{} (max {}) -> {} | {} external subtitle(s), subtitle choice {subtitle:?}",
            stream.identity.method.as_str(),
            if stream.transcode_reasons.is_empty() { String::new() } else { format!(" ({})", stream.transcode_reasons.join(", ")) },
            settings.resolution.label(),
            settings.bitrate.label(),
            settings.compatibility.label(),
            if settings.force_hdr_transcode { " · force SDR" } else { "" },
            format_bitrate(max_bitrate),
            redact(&stream.url),
            stream.subtitles.len()
        ));
        let Some(player) = &self.player else { return };
        if let Err(e) = player.load_with_subtitles(&stream.url, start, &[self.jf.media_auth_header()], stream.subtitles, subtitle) {
            self.error = Some(format!("{e:#}"));
            cx.notify();
            return;
        }
        // Only now: after a failed restart the menu must keep showing what still plays.
        self.settings = Some(settings);
        let previous = self.stream.replace(ActiveStream {
            identity: stream.identity.clone(),
            transcode_reasons: stream.transcode_reasons,
            max_bitrate,
        });
        self.pending_report = Some(match self.pending_report {
            // Restarted before the first stream ever started: its start is still owed.
            Some(PendingReport::Start) => PendingReport::Start,
            _ if previous.is_none() => PendingReport::Start,
            _ => PendingReport::Restart,
        });
        if let Some(ActiveStream { identity: previous, .. }) = previous
            && previous.method == PlayMethod::Transcode
            && let Some(play_session_id) = previous.play_session_id
            && stream.identity.play_session_id.as_deref() != Some(play_session_id.as_str())
        {
            let jf = self.jf.clone();
            cx.background_executor()
                .spawn(async move {
                    match jf.stop_transcoding(&play_session_id) {
                        Ok(()) => proof_log(format!("player: stopped the previous transcode ({play_session_id})")),
                        Err(e) => log::warn!("stopping the previous transcode: {e:#}"),
                    }
                })
                .detach();
        }
        cx.notify();
    }

    /// Restart the stream at the current position with new settings, for this playback only.
    fn change_quality(&mut self, settings: PlaybackSettings, cx: &mut Context<Self>) {
        self.open_menu = None;
        if self.settings.as_ref() == Some(&settings) {
            cx.notify();
            return;
        }
        let Some(player) = &self.player else { return };
        let position = playing_position(player, &player.status()).unwrap_or(self.start);
        self.start = position;
        proof_log(format!(
            "player: quality changed to {} · {} for this playback, restarting at {position:.2}s",
            settings.resolution.label(),
            settings.bitrate.label()
        ));
        self.flash(format!("Quality: {} · {}", settings.resolution.label(), settings.bitrate.label()), cx);
        let measured_bitrate = self
            .stream
            .as_ref()
            .filter(|_| settings.bitrate == Bitrate::Auto && self.settings.as_ref().is_some_and(|current| current.bitrate == Bitrate::Auto))
            .map(|stream| stream.max_bitrate);
        self.open_stream(position, Some(settings), measured_bitrate, cx);
    }

    fn load_adjacent_episodes(jf: Jellyfin, series_id: String, item_id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let episodes = cx.background_executor().spawn(async move { jf.episodes(&series_id) }).await;
            let episodes = match episodes {
                Ok(episodes) => episodes,
                Err(e) => {
                    log::warn!("could not load the series' episodes: {e:#}");
                    return;
                }
            };
            this.update(cx, |view, cx| {
                (view.previous_episode, view.next_episode) = adjacent_episodes(&episodes, &item_id);
                let describe = |episode: &Option<Item>| {
                    episode.as_ref().map_or_else(|| "none".to_string(), |e| format!("'{}' {} ({})", e.name, e.subtitle(), e.id))
                };
                proof_log(format!(
                    "episodes: {} in series, previous={}, next={}",
                    episodes.len(),
                    describe(&view.previous_episode),
                    describe(&view.next_episode)
                ));
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn go_to_episode(&mut self, episode: Option<Item>, missing: &str, cx: &mut Context<Self>) {
        let Some(episode) = episode else {
            proof_log(format!("episodes: {missing}"));
            self.flash(missing, cx);
            return;
        };
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(t) = self.shutdown(cx) {
            t.detach();
        }
        self.frame = None;
        proof_log(format!(
            "episodes: switching from '{}' {} to '{}' {}",
            self.item.name,
            self.item.subtitle(),
            episode.name,
            episode.subtitle()
        ));
        cx.emit(PlayEpisode(episode));
    }

    fn select_subtitle(&mut self, id: Option<i64>, cx: &mut Context<Self>) {
        let Some(player) = &self.player else { return };
        self.open_menu = None;
        if let Err(e) = player.select_subtitle(id) {
            self.flash(format!("{e:#}"), cx);
            return;
        }
        // The cached list catches up on the event thread; show the choice now.
        for track in self.tracks.iter_mut().filter(|t| t.kind == "sub") {
            track.selected = Some(track.id) == id;
        }
        let chosen = self.tracks.iter().find(|t| t.kind == "sub" && Some(t.id) == id);
        let label = chosen.map_or_else(|| "Off".to_string(), |t| t.label.clone());
        let preference = SubtitlePreference::from_track(chosen);
        proof_log(format!("player: subtitle selected -> {label}, saving {preference:?}"));
        self.flash(format!("Subtitles: {label}"), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Preferences::save_subtitle(preference) })
                .await;
            if let Err(e) = result {
                log::warn!("saving subtitle preference: {e:#}");
                this.update(cx, |view, cx| view.flash(format!("Couldn't remember subtitles: {e}"), cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn subtitle_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let subtitles: Vec<_> = self.tracks.iter().filter(|t| t.kind == "sub").collect();
        let selected = subtitles.iter().find(|t| t.selected).map(|t| t.id);
        let mut options = vec![(None, "Off".to_string())];
        options.extend(subtitles.iter().map(|t| (Some(t.id), t.label.clone())));
        div()
            .id("subtitle-menu")
            .absolute()
            .bottom(px(30.))
            .left_0()
            .min_w(px(280.))
            .py_1()
            .rounded_lg()
            .bg(surface())
            .border_1()
            .border_color(surface_hi())
            .text_sm()
            .text_color(text())
            .occlude()
            .children(options.into_iter().enumerate().map(|(index, (id, label))| {
                div()
                    .id(("subtitle-option", index))
                    .px_3()
                    .py_1p5()
                    .flex()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(surface_hi()))
                    .child(div().w(px(14.)).text_color(accent()).child(if id == selected { "✓" } else { "" }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_subtitle(id, cx)))
            }))
            .when(subtitles.is_empty(), |d| {
                d.child(div().px_3().py_1p5().text_color(text_dim()).child("No subtitles for this video"))
            })
            .into_any_element()
    }

    fn toggle_menu(&mut self, menu: Menu) {
        self.open_menu = if self.open_menu == Some(menu) { None } else { Some(menu) };
    }

    /// "Direct Play · max 23 Mbps" or "Transcode (VideoResolutionNotSupported) · max 4 Mbps".
    fn stream_description(&self) -> Option<String> {
        let stream = self.stream.as_ref()?;
        let method = match stream.identity.method {
            PlayMethod::DirectPlay => "Direct Play".to_string(),
            PlayMethod::DirectStream => "Direct Stream".to_string(),
            PlayMethod::Transcode if stream.transcode_reasons.is_empty() => "Transcode".to_string(),
            PlayMethod::Transcode => format!("Transcode ({})", stream.transcode_reasons.join(", ")),
        };
        Some(format!("{method} · max {}", format_bitrate(stream.max_bitrate)))
    }

    fn quality_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(current) = self.settings.clone() else {
            return div().into_any_element();
        };
        let option = |id: (&'static str, usize), label: String, chosen: bool, settings: PlaybackSettings| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .flex()
                .gap_2()
                .cursor_pointer()
                .hover(|s| s.bg(surface_hi()))
                .child(div().w(px(14.)).text_color(accent()).child(if chosen { "✓" } else { "" }))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.change_quality(settings.clone(), cx)))
        };
        let column = |title: &'static str| {
            div()
                .flex()
                .flex_col()
                .min_w(px(150.))
                .child(div().px_3().pb_1().text_xs().text_color(text_dim()).child(title))
        };
        let resolutions = column("Resolution").children(MaxResolution::ALL.into_iter().enumerate().map(|(index, resolution)| {
            let settings = PlaybackSettings { resolution, ..current.clone() };
            option(("quality-resolution", index), resolution.label().to_string(), resolution == current.resolution, settings)
        }));
        let bitrates = column("Bitrate").children(Bitrate::ALL.into_iter().enumerate().map(|(index, bitrate)| {
            let settings = PlaybackSettings { bitrate, ..current.clone() };
            option(("quality-bitrate", index), bitrate.label(), bitrate == current.bitrate, settings)
        }));
        div()
            .id("quality-menu")
            .absolute()
            .bottom(px(30.))
            .left_0()
            .py_2()
            .rounded_lg()
            .bg(surface())
            .border_1()
            .border_color(surface_hi())
            .text_sm()
            .text_color(text())
            .occlude()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().flex().child(resolutions).child(bitrates))
            .child(
                div()
                    .px_3()
                    .pt_1()
                    .text_xs()
                    .text_color(text_dim())
                    .child("Applies to this video only; defaults are in Settings."),
            )
            .into_any_element()
    }

    fn refresh_status(&mut self) -> Status {
        if let Some(p) = &self.player {
            self.status = p.status();
            self.tracks = p.tracks();
        }
        let now = Instant::now();
        let (t0, n0) = self.presented_window;
        let dt = now.duration_since(t0).as_secs_f64();
        if dt >= 1.0 {
            self.presented_fps = (self.presented - n0) as f64 / dt;
            self.presented_window = (now, self.presented);
        }
        // Read by scripts/bench.sh to prove optimisations keep every frame.
        if log::log_enabled!(target: "ztriton::stats", log::Level::Debug)
            && let Some(p) = &self.player
        {
            log::debug!(
                target: "ztriton::stats",
                "time-pos={:.2} mpv_frames={} presented={} presented_fps={:.2} vo_drops={} decoder_drops={} hwdec={}",
                self.status.position,
                p.frames_rendered(),
                self.presented,
                self.presented_fps,
                p.property_string("frame-drop-count"),
                p.property_string("decoder-frame-drop-count"),
                self.status.hwdec
            );
        }
        if crate::proof_mode() {
            let s = &self.status;
            proof_log(format!(
                "player: time-pos={:.2}s / {:.1}s paused={} muted={} codec={} {}x{} hwdec={} | mpv frames={} | gpui frames presented={} ({:.1} fps)",
                s.position,
                s.duration,
                s.paused,
                s.muted,
                s.video_codec,
                s.width,
                s.height,
                s.hwdec,
                self.player.as_ref().map(|p| p.frames_rendered()).unwrap_or(0),
                self.presented,
                self.presented_fps
            ));
        }
        self.status.clone()
    }

    /// Scripted interaction used to produce verifiable evidence.
    fn spawn_proof_script(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let t = |s: u64| Duration::from_secs(s);
            let exec = cx.background_executor().clone();
            let timer = |d| exec.timer(d);
            timer(t(8)).await;
            this.update(cx, |v, _| v.dump_presented_frame("gpui_frame_1.png")).log_err();
            proof_log("proof: marker playing-1");
            timer(t(4)).await;
            // Server-side confirmation via /Sessions.
            if let Ok(jf) = this.update(cx, |v, _| v.jf.clone()) {
                let res = exec.spawn(async move { jf.sessions_json() }).await;
                log_server_session(res, "sessions.json");
            }
            timer(t(3)).await;
            this.update(cx, |v, cx| {
                if let Some(p) = &v.player {
                    let target = v.status.duration * 0.60;
                    proof_log(format!("proof: seeking to 60% = {target:.1}s"));
                    p.seek_absolute(target);
                    v.flash("Seek → 60%", cx);
                }
            })
            .log_err();
            timer(t(6)).await;
            this.update(cx, |v, _| v.dump_presented_frame("gpui_frame_2.png")).log_err();
            proof_log("proof: marker playing-2");
            this.update(cx, |v, cx| {
                if let Some(p) = &v.player {
                    p.set_paused(true);
                }
                v.last_activity = Instant::now();
                cx.notify();
            })
            .log_err();
            timer(t(2)).await;
            proof_log("proof: marker paused-overlay");
            timer(t(4)).await;
            this.update(cx, |v, cx| {
                if let Some(p) = &v.player {
                    p.set_paused(false);
                }
                cx.notify();
            })
            .log_err();
            timer(t(3)).await;
            // In-player quality change: same spot, this playback only.
            this.update(cx, |v, cx| {
                if let Some(settings) = v.settings.clone() {
                    let resolution = MaxResolution::Sd480;
                    v.change_quality(PlaybackSettings { resolution, ..settings }, cx);
                }
            })
            .log_err();
            timer(t(12)).await;
            this.update(cx, |v, _| {
                let status = v.refresh_status();
                proof_log(format!(
                    "proof: after the quality change mpv plays {}x{} {} at {:.2}s; {}",
                    status.width,
                    status.height,
                    status.video_codec,
                    status.position,
                    v.stream_description().unwrap_or_default()
                ));
            })
            .log_err();
            if let Ok(jf) = this.update(cx, |v, _| v.jf.clone()) {
                let res = exec.spawn(async move { jf.sessions_json() }).await;
                log_server_session(res, "sessions_after_quality_change.json");
            }
            match Preferences::load() {
                Ok(preferences) => proof_log(format!(
                    "proof: saved defaults after the in-player change: {} · {}",
                    preferences.playback.resolution.label(),
                    preferences.playback.bitrate.label()
                )),
                Err(e) => proof_log(format!("proof: could not read preferences: {e:#}")),
            }
            this.update(cx, |v, _| v.dump_presented_frame("gpui_frame_3.png")).log_err();
            proof_log("proof: done");
            if std::env::var("ZTRITON_PROOF_QUIT").is_ok() {
                this.update(cx, |v, cx| v.close(cx)).log_err();
                timer(t(2)).await;
                cx.update(|cx| cx.quit()).log_err();
            }
        })
        .detach();
    }

    /// Dump the pixel buffer GPUI is currently drawing.
    fn dump_presented_frame(&self, name: &str) {
        let Some(frame) = &self.frame else {
            proof_log(format!("proof: no presented frame for {name}"));
            return;
        };
        let path = crate::proof_dir().join(name);
        match frame.save_png(&path) {
            Ok(()) => proof_log(format!(
                "proof: dumped presented NV12 surface {}x{} (mpv frame seq {}) at time-pos {:.2}s -> {}",
                frame.width, frame.height, frame.seq, self.status.position, path.display()
            )),
            Err(e) => proof_log(format!("proof: dump failed: {e:#}")),
        }
    }

    fn flash(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.osd = Some((msg.into(), Instant::now()));
        self.last_activity = Instant::now();
        cx.notify();
    }

    fn with_player(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&Player) -> String) {
        if let Some(p) = &self.player {
            let msg = f(p);
            self.tracks = p.tracks();
            self.flash(msg, cx);
        }
    }

    fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            let was = p.status().paused;
            p.toggle_pause();
            if was { "Play".into() } else { "Paused".into() }
        });
    }
    fn seek_fwd(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.seek_relative(10.0);
            "+10s".into()
        });
    }
    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.seek_relative(-10.0);
            "−10s".into()
        });
    }
    fn seek_fwd_big(&mut self, _: &SeekForwardBig, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.seek_relative(60.0);
            "+60s".into()
        });
    }
    fn seek_back_big(&mut self, _: &SeekBackBig, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.seek_relative(-60.0);
            "−60s".into()
        });
    }
    fn vol_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.set_volume(p.volume() + 5);
            format!("Volume {}", p.volume())
        });
    }
    fn vol_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.set_volume(p.volume() - 5);
            format!("Volume {}", p.volume())
        });
    }
    fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            p.cycle("aid");
            track_label(p, "audio", "Audio")
        });
    }
    fn cycle_subs(&mut self, _: &CycleSubs, _: &mut Window, cx: &mut Context<Self>) {
        let mut options = vec![None];
        options.extend(self.tracks.iter().filter(|t| t.kind == "sub").map(|t| Some(t.id)));
        let current = self.tracks.iter().find(|t| t.kind == "sub" && t.selected).map(|t| t.id);
        let position = options.iter().position(|id| *id == current).unwrap_or(0);
        let next = options.get((position + 1) % options.len()).copied().flatten();
        self.select_subtitle(next, cx);
    }
    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        self.with_player(cx, |p| {
            let was = p.is_muted();
            p.toggle_mute();
            if was { format!("Volume {}", p.volume()) } else { "Muted".into() }
        });
    }
    fn previous_episode(&mut self, _: &PreviousEpisode, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_episode(self.previous_episode.clone(), "No previous episode", cx);
    }
    fn next_episode(&mut self, _: &NextEpisode, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_episode(self.next_episode.clone(), "No next episode", cx);
    }
    fn fullscreen(&mut self, _: &Fullscreen, window: &mut Window, _: &mut Context<Self>) {
        window.toggle_fullscreen();
    }
    fn on_close(&mut self, _: &ClosePlayer, _: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }

    /// Stop playback immediately, then tear mpv down and report the stop to
    /// Jellyfin off the UI thread. Returns the background task (if any).
    fn shutdown(&mut self, cx: &mut Context<Self>) -> Option<gpui::Task<()>> {
        let p = self.player.take()?;
        let status = p.status();
        // Closed before the first frame: the stop report below makes the
        // server's copy newer (and 0 if under its minimum), so re-save `start`.
        let pos = playing_position(&p, &status).unwrap_or(self.start);
        p.stop(); // cheap async command: audio/video stop right now
        self.save_position(pos, status.duration);
        proof_log(format!("player: closing at {pos:.2}s, reporting stop to Jellyfin"));
        let jf = self.jf.clone();
        let id = self.item.id.clone();
        // Dropping the request task stops a stream that has not started yet.
        self.stream_task = None;
        let stream = self.stream.as_ref().map(|stream| stream.identity.clone()).unwrap_or_default();
        Some(cx.background_executor().spawn(async move {
            // Report before tearing mpv down: on quit GPUI waits only ~100 ms for this task.
            if let Err(e) = jf.report_stopped(&id, pos, &stream) {
                log::warn!("report_stopped: {e}");
            }
            drop(p); // joins mpv threads, destroys the core
        }))
    }

    fn save_position(&self, position: f64, duration: f64) {
        match ztriton::resume::remember(&self.jf.user_id, &self.item.id, position, duration) {
            Ok(()) => proof_log(format!("resume: saved '{}' at {position:.2}s of {duration:.1}s", self.item.name)),
            Err(e) => log::warn!("could not save the resume position: {e:#}"),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(t) = self.shutdown(cx) {
            t.detach();
        }
        self.frame = None;
        cx.emit(Closed);
    }

    fn take_new_frame(&mut self) {
        let Some(p) = &self.player else { return };
        let Some(frame) = p.latest_frame() else { return };
        if self.frame.as_ref().is_some_and(|shown| shown.seq == frame.seq) {
            return;
        }
        if self.presented == 0 {
            proof_log(format!("player: first frame presented via GPUI surface ({}x{})", frame.width, frame.height));
        }
        self.frame = Some(frame);
        self.presented += 1;
    }

    fn controls(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let st = &self.status;
        let frac = if st.duration > 0.0 { (st.position / st.duration).clamp(0.0, 1.0) } else { 0.0 };
        let vw: f32 = window.viewport_size().width.into();
        let bar_w = (vw - BAR_MARGIN * 2.0).max(10.0);
        let remaining = (st.duration - st.position).max(0.0);

        let btn = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_lg()
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0xffffff33)))
                .child(label)
        };

        let tracks = &self.tracks;
        let audio = tracks.iter().find(|t| t.kind == "audio" && t.selected).map(|t| t.label.clone()).unwrap_or("—".into());
        let sub = tracks.iter().find(|t| t.kind == "sub" && t.selected).map(|t| t.label.clone()).unwrap_or("Off".into());

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .justify_between()
            // The menu is positioned outside its parent's bounds, so mouse-down-out on the
            // parent would fire for clicks on the menu itself; dismiss via a backdrop instead.
            .when(self.open_menu.is_some(), |d| {
                d.child(div().absolute().top_0().left_0().size_full().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.open_menu = None;
                        cx.notify();
                    }),
                ))
            })
            // Top bar
            .child(
                div()
                    .w_full()
                    .px(px(BAR_MARGIN))
                    .pt_6()
                    .pb_10()
                    .bg(linear_gradient(180., linear_color_stop(rgba(0x000000cc), 0.), linear_color_stop(rgba(0x00000000), 1.)))
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        btn("close", "✕").on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_xl().font_weight(FontWeight::BOLD).child(self.item.name.clone()))
                            .child(div().text_sm().text_color(text_dim()).child(self.item.subtitle())),
                    )
                    .child(div().flex_1())
                    .child(fullscreen_button("player-fullscreen", window.is_fullscreen())),
            )
            // Bottom bar
            .child(
                div()
                    .w_full()
                    .px(px(BAR_MARGIN))
                    .pt_12()
                    .pb_6()
                    .bg(linear_gradient(0., linear_color_stop(rgba(0x000000dd), 0.), linear_color_stop(rgba(0x00000000), 1.)))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        // Scrubber
                        div()
                            .id("scrubber")
                            .w(px(bar_w))
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                    let x: f32 = ev.position.x.into();
                                    let f = ((x - BAR_MARGIN) / bar_w).clamp(0.0, 1.0) as f64;
                                    if let Some(p) = &this.player {
                                        p.seek_percent(f * 100.0);
                                    }
                                    this.flash(format!("Seek → {:.0}%", f * 100.0), cx);
                                }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h(px(5.))
                                    .rounded_full()
                                    .bg(rgba(0xffffff40))
                                    .child(div().h_full().rounded_full().w(px(bar_w * frac as f32)).bg(accent()))
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(-5.))
                                            .left(px(bar_w * frac as f32 - 7.))
                                            .size(px(15.))
                                            .rounded_full()
                                            .bg(text()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().w(px(160.)).text_sm().text_color(text_dim()).child(fmt_time(st.position)))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .when(self.previous_episode.is_some(), |d| {
                                        d.child(btn("previous-episode", "⏮").on_click(
                                            cx.listener(|this, _, w, cx| this.previous_episode(&PreviousEpisode, w, cx)),
                                        ))
                                    })
                                    .child(btn("back10", "↺").on_click(cx.listener(|this, _, w, cx| this.seek_back(&SeekBack, w, cx))))
                                    .child(
                                        btn("playpause", if st.paused { "▶" } else { "❚❚" })
                                            .size(px(56.))
                                            .bg(rgba(0xffffff26))
                                            .on_click(cx.listener(|this, _, w, cx| this.toggle_pause(&TogglePause, w, cx))),
                                    )
                                    .child(btn("fwd10", "↻").on_click(cx.listener(|this, _, w, cx| this.seek_fwd(&SeekForward, w, cx))))
                                    .when(self.next_episode.is_some(), |d| {
                                        d.child(btn("next-episode", "⏭").on_click(
                                            cx.listener(|this, _, w, cx| this.next_episode(&NextEpisode, w, cx)),
                                        ))
                                    }),
                            )
                            .child(div().w(px(160.)).flex().justify_end().text_sm().text_color(text_dim()).child(format!("−{}", fmt_time(remaining)))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .text_xs()
                            .text_color(text_dim())
                            .child(
                                div()
                                    .id("mute")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(if st.muted { accent() } else { rgba(0xffffff1a).into() })
                                    .when(st.muted, |d| d.text_color(bg()))
                                    .cursor_pointer()
                                    .child(if st.muted { "Muted" } else { "Sound on" })
                                    .on_click(cx.listener(|this, _, w, cx| this.toggle_mute(&ToggleMute, w, cx))),
                            )
                            .child(
                                div()
                                    .id("audio")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgba(0xffffff1a))
                                    .cursor_pointer()
                                    .child(format!("Audio: {audio}"))
                                    .on_click(cx.listener(|this, _, w, cx| this.cycle_audio(&CycleAudio, w, cx))),
                            )
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        div()
                                            .id("subs")
                                            // Occluded from the dismiss backdrop, so a click here
                                            // toggles the menu rather than closing and reopening it.
                                            .occlude()
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(if self.open_menu == Some(Menu::Subtitles) { surface_hi() } else { rgba(0xffffff1a).into() })
                                            .cursor_pointer()
                                            .child(format!("CC {sub}"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_menu(Menu::Subtitles);
                                                this.last_activity = Instant::now();
                                                cx.notify();
                                            })),
                                    )
                                    .when(self.open_menu == Some(Menu::Subtitles), |d| d.child(self.subtitle_menu(cx))),
                            )
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        div()
                                            .id("quality")
                                            .occlude()
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(if self.open_menu == Some(Menu::Quality) { surface_hi() } else { rgba(0xffffff1a).into() })
                                            .cursor_pointer()
                                            .child(match &self.settings {
                                                Some(settings) => format!("Quality: {} · {}", settings.resolution.label(), settings.bitrate.label()),
                                                None => "Quality".to_string(),
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_menu(Menu::Quality);
                                                this.last_activity = Instant::now();
                                                cx.notify();
                                            })),
                                    )
                                    .when(self.open_menu == Some(Menu::Quality), |d| d.child(self.quality_menu(cx))),
                            )
                            .child(div().flex_grow())
                            .when_some(self.stream_description(), |d, description| d.child(description))
                            .child(format!(
                                "{} · {}x{} · {} · {:.0} fps",
                                if st.video_codec.is_empty() { "…".to_string() } else { st.video_codec.split(" / ").next().unwrap_or("").to_string() },
                                st.width,
                                st.height,
                                if st.hwdec.is_empty() || st.hwdec == "no" { "sw decode".to_string() } else { st.hwdec.clone() },
                                self.presented_fps
                            )),
                    ),
            )
            .into_any_element()
    }
}

/// Where playback is, or `None` until mpv renders a frame: before that the
/// file may know its duration yet still report 0 instead of the `start` it is
/// seeking to, and saving that would erase the resume position.
fn playing_position(player: &Player, status: &Status) -> Option<f64> {
    (status.duration > 0.0 && player.frames_of_current_file() > 0).then_some(status.position)
}

/// Write the server's /Sessions answer to `file` in the proof directory and log
/// how the server says it is streaming.
fn log_server_session(sessions: anyhow::Result<serde_json::Value>, file: &str) {
    let json = match sessions {
        Ok(json) => json,
        Err(e) => {
            proof_log(format!("proof: sessions query failed: {e}"));
            return;
        }
    };
    match serde_json::to_string_pretty(&json) {
        Ok(pretty) => std::fs::write(crate::proof_dir().join(file), pretty).log_err(),
        Err(e) => log::warn!("sessions json: {e}"),
    }
    let field = |pointer: &str| json.pointer(&format!("/0{pointer}")).cloned().unwrap_or_default();
    proof_log(format!(
        "proof: server /Sessions -> client={} device={} NowPlayingItem={} PositionTicks={} PlayMethod={} TranscodingInfo: {}x{} {} {} bitrate={} reasons={}",
        field("/Client"),
        field("/DeviceName"),
        field("/NowPlayingItem/Name"),
        field("/PlayState/PositionTicks"),
        field("/PlayState/PlayMethod"),
        field("/TranscodingInfo/Width"),
        field("/TranscodingInfo/Height"),
        field("/TranscodingInfo/VideoCodec"),
        field("/TranscodingInfo/AudioCodec"),
        field("/TranscodingInfo/Bitrate"),
        field("/TranscodingInfo/TranscodeReasons")
    ));
}

fn track_label(p: &Player, kind: &str, name: &str) -> String {
    let t = p.tracks().into_iter().find(|t| t.kind == kind && t.selected);
    match t {
        Some(t) => format!("{name}: {}", t.label),
        None => format!("{name}: Off"),
    }
}

impl Focusable for PlayerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.take_new_frame();
        if let Some(p) = &self.player {
            self.status = p.status(); // cached snapshot, no mpv round-trip
        }

        let show_controls = self.status.paused
            || self.open_menu.is_some()
            || self.last_activity.elapsed() < Duration::from_secs(3) || self.frame.is_none();
        let osd = self
            .osd
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_millis(1200))
            .map(|(m, _)| m.clone());

        let video: AnyElement = match (&self.frame, &self.error) {
            (_, Some(err)) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui::red())
                .child(format!("Playback error: {err}"))
                .into_any_element(),
            (Some(frame), _) => gpui::surface(frame.pixel_buffer.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            (None, _) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(text_dim())
                .child(if self.status.buffering { "Buffering…" } else { "Loading…" })
                .into_any_element(),
        };

        div()
            .id("player")
            .key_context("Player")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::seek_fwd))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_fwd_big))
            .on_action(cx.listener(Self::seek_back_big))
            .on_action(cx.listener(Self::vol_up))
            .on_action(cx.listener(Self::vol_down))
            .on_action(cx.listener(Self::cycle_audio))
            .on_action(cx.listener(Self::cycle_subs))
            .on_action(cx.listener(Self::on_close))
            .on_action(cx.listener(Self::fullscreen))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::previous_episode))
            .on_action(cx.listener(Self::next_episode))
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                this.last_activity = Instant::now();
                cx.notify();
            }))
            .relative()
            .size_full()
            .bg(gpui::black())
            .text_color(text())
            .child(video)
            .when(show_controls, |d| d.child(self.controls(window, cx)))
            .when_some(osd, |d, msg| {
                d.child(
                    div().absolute().top(px(90.)).left_0().w_full().flex().justify_center().child(
                        div()
                            .px_5()
                            .py_2()
                            .rounded_lg()
                            .bg(rgba(0x000000aa))
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(msg)),
                    ),
                )
            })
    }
}
