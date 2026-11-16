//! mpv playback core.
//!
//! * `libmpv2::Mpv` drives playback (options, commands, events).
//! * Player state is *observed* (`observe_property`) on the event thread and
//!   cached, so UI reads never block on the mpv core lock.
//! * Video frames come from mpv's OpenGL render API on a dedicated render
//!   thread that only calls `mpv_render_*` functions (as required by
//!   render.h's threading rules) and owns an offscreen GL context. Each frame
//!   stays on the GPU: it is converted to an NV12 IOSurface (see
//!   [`crate::gpu_video`]) and published as a [`VideoFrame`] that GPUI draws
//!   with Metal.

use anyhow::{Result, anyhow, bail};
use libmpv2::{
    Format, Mpv, SetData,
    events::{Event, PropertyData},
};
use core_video::pixel_buffer::{CVPixelBuffer, kCVPixelBufferLock_ReadOnly};
use libmpv2_sys as sys;
use parking_lot::{Condvar, Mutex};
use crate::gpu_video::{GlContext, Nv12Converter};
use std::ffi::{CStr, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Largest frame we ask mpv to render. 4K sources are scaled down by mpv on
/// the GPU, which keeps each frame buffer at 3 MB.
const MAX_W: u32 = 1920;
const MAX_H: u32 = 1080;

/// One display-ready frame: an IOSurface-backed NV12 (BT.601 full range)
/// pixel buffer that GPUI's `surface()` element draws directly.
pub struct VideoFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The frame's pixels, never written again after publication.
    pub pixel_buffer: CVPixelBuffer,
    /// Increases by one for every rendered frame.
    pub seq: u64,
}

// SAFETY: CVPixelBuffer is a reference-counted CoreFoundation object whose
// retain/release are thread-safe. The render thread finishes writing (and waits
// for the GPU) before publishing, and the pool only recycles a buffer once every
// reference is gone, so shared access is read-only.
#[expect(unsafe_code, reason = "CVPixelBuffer is not marked Send by core-video")]
unsafe impl Send for VideoFrame {}
// SAFETY: see the Send impl; `&VideoFrame` only allows reads.
#[expect(unsafe_code, reason = "CVPixelBuffer is not marked Sync by core-video")]
unsafe impl Sync for VideoFrame {}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// One entry of mpv's track list.
#[derive(Clone, Debug, Default)]
pub struct Track {
    /// mpv track id, unique per track type within the current file.
    pub id: i64,
    /// `"audio"`, `"sub"` or `"video"`.
    pub kind: String,
    /// Language tag as reported by mpv (e.g. `"eng"`), possibly empty.
    pub language: String,
    /// Track title, possibly empty.
    pub title: String,
    /// Human-readable name for the UI.
    pub label: String,
    /// Whether this track is currently playing.
    pub selected: bool,
}

/// A subtitle file served separately from the video stream (e.g. a Jellyfin
/// sidecar `.srt`), added to mpv once the video has loaded.
#[derive(Clone, Debug)]
pub struct ExternalSubtitle {
    /// HTTP(S) URL of the subtitle file.
    pub url: String,
    /// Track title shown in the subtitle picker.
    pub title: String,
    /// Language tag (e.g. `"vie"`), possibly empty.
    pub language: String,
}

/// The subtitle choice remembered across playbacks. Tracks are matched by
/// language and title rather than id, because ids differ between files.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubtitlePreference {
    /// Subtitles turned off.
    Off,
    /// A specific track.
    Track {
        /// The track's language tag.
        language: String,
        /// The track's title.
        title: String,
    },
}

impl SubtitlePreference {
    /// The preference that reproduces choosing `track` (`None` = off).
    pub fn from_track(track: Option<&Track>) -> Self {
        match track {
            Some(track) => Self::Track {
                language: track.language.clone(),
                title: track.title.clone(),
            },
            None => Self::Off,
        }
    }
}

/// `Some(None)` = turn subtitles off, `Some(Some(id))` = select that track,
/// `None` = nothing matches the preference, leave mpv's choice alone.
pub fn choose_subtitle(tracks: &[Track], preference: &SubtitlePreference) -> Option<Option<i64>> {
    let (language, title) = match preference {
        SubtitlePreference::Off => return Some(None),
        SubtitlePreference::Track { language, title } => (language, title),
    };
    let subs = || tracks.iter().filter(|t| t.kind == "sub");
    let exact = subs().find(|t| &t.language == language && &t.title == title);
    let same_language = || subs().find(|t| !language.is_empty() && &t.language == language);
    exact.or_else(same_language).map(|t| Some(t.id))
}

struct PendingSubtitles {
    external: Vec<ExternalSubtitle>,
    preference: Option<SubtitlePreference>,
}

/// Snapshot of playback state, kept current by mpv property observers.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Playback position in seconds.
    pub position: f64,
    /// File duration in seconds (0 until known).
    pub duration: f64,
    /// Whether playback is paused.
    pub paused: bool,
    /// Whether the end of the file was reached.
    pub eof: bool,
    /// Whether playback is waiting for the network cache.
    pub buffering: bool,
    /// Whether audio is muted.
    pub muted: bool,
    /// Volume in percent.
    pub volume: f64,
    /// Video codec description.
    pub video_codec: String,
    /// Source video width in pixels.
    pub width: i64,
    /// Source video height in pixels.
    pub height: i64,
    /// Active hardware decoder, `"no"` or empty for software decoding.
    pub hwdec: String,
}

/// Options fixed when a [`Player`] is created.
#[derive(Clone, Debug)]
pub struct PlayerOptions {
    /// Disable audio output (headless tests).
    pub no_audio: bool,
    /// Start muted (audio still decoded; toggle with `toggle_mute`).
    pub muted: bool,
    /// Initial volume in percent.
    pub volume: i64,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            no_audio: false,
            muted: false,
            volume: 100,
        }
    }
}

/// Frame notification. Runs on the render thread: it MUST be non-blocking and
/// must not call into `Player` (send on a channel / set a flag, nothing else).
type FrameCallback = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    latest: Mutex<Option<Arc<VideoFrame>>>,
    frames_rendered: AtomicU64,
    /// `frames_rendered` when the current file loaded, or `u64::MAX` from the
    /// moment another file is requested until it has loaded, so frames of the
    /// previous file never count as the new one playing.
    frames_before_current_file: AtomicU64,
    /// Display size of the video, packed `(w << 32) | h`; written by the event
    /// thread, read by the render thread (which may not call client APIs).
    dsize: AtomicU64,
    status: Mutex<Status>,
    tracks: Mutex<Vec<Track>>,
    /// Applied by the event thread on the next `FileLoaded`.
    pending_subtitles: Mutex<Option<PendingSubtitles>>,
    /// Set by mpv's update callback, consumed by the render thread.
    dirty: Mutex<bool>,
    cv: Condvar,
    stop: AtomicBool,
    on_frame: Mutex<Option<FrameCallback>>,
    events: Mutex<Vec<String>>,
    started: Instant,
}

impl Shared {
    fn log_event(&self, msg: String) {
        let line = format!("[{:>8.3}s] {msg}", self.started.elapsed().as_secs_f64());
        log::info!("mpv: {msg}");
        self.events.lock().push(line);
    }
}

/// An mpv instance with its render and event threads.
pub struct Player {
    mpv: Arc<Mpv>,
    shared: Arc<Shared>,
    render_thread: Option<JoinHandle<()>>,
    event_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Raw mpv client API version of the *loaded* libmpv dylib.
#[expect(unsafe_code, reason = "libmpv2 has no safe wrapper for mpv_client_api_version")]
pub fn client_api_version() -> (u64, u64) {
    // SAFETY: takes no arguments and only reads a constant from the loaded library.
    let v = unsafe { sys::mpv_client_api_version() } as u64;
    (v >> 16, v & 0xffff)
}

// Observed property ids.
const P_TIME: u64 = 1;
const P_DURATION: u64 = 2;
const P_PAUSE: u64 = 3;
const P_EOF: u64 = 4;
const P_CACHE: u64 = 5;
const P_CODEC: u64 = 6;
const P_W: u64 = 7;
const P_H: u64 = 8;
const P_HWDEC: u64 = 9;
const P_DW: u64 = 10;
const P_DH: u64 = 11;
const P_MUTE: u64 = 12;
const P_VOLUME: u64 = 13;
const P_TRACKS: u64 = 14;
const P_AID: u64 = 15;
const P_SID: u64 = 16;

impl Player {
    /// Create mpv and start its render and event threads.
    pub fn new(opts: PlayerOptions) -> Result<Self> {
        let (maj, min) = client_api_version();
        if maj < 2 || (maj == 2 && min < 6) {
            bail!("libmpv client API {maj}.{min} is too old; mpv 0.42 (API >= 2.6) required");
        }

        let mpv = Mpv::with_initializer(|init| {
            init.set_option("vo", "libmpv")?;
            // Zero-copy VideoToolbox decoding into GL textures; auto-safe
            // falls back to software decoding for unsupported codecs.
            init.set_option("hwdec", "auto-safe")?;
            init.set_option("keep-open", "yes")?;
            init.set_option("idle", "yes")?;
            init.set_option("terminal", "no")?;
            init.set_option("config", "no")?;
            init.set_option("input-default-bindings", "no")?;
            init.set_option("osc", "no")?;
            init.set_option("ytdl", "no")?;
            // Built-in Lua scripts (each its own VM and thread) and input
            // handlers that only matter for mpv's own window; the UI uses none.
            for script in [
                "load-scripts",
                "load-stats-overlay",
                "load-console",
                "load-select",
                "load-positioning",
                "load-commands",
                "load-context-menu",
                "load-auto-profiles",
                "input-builtin-drag-and-drop",
            ] {
                init.set_option(script, "no")?;
            }
            init.set_option("cache", "yes")?;
            // 20 s ahead covers network hiccups; the caps bound RAM for
            // high-bitrate files (mpv keeps 50 MiB behind by default).
            init.set_option("demuxer-max-bytes", "32MiB")?;
            init.set_option("demuxer-max-back-bytes", "8MiB")?;
            init.set_option("demuxer-readahead-secs", "20")?;
            init.set_option("sub-auto", "fuzzy")?;
            init.set_option("volume", opts.volume)?;
            init.set_option("mute", if opts.muted { "yes" } else { "no" })?;
            if opts.no_audio {
                init.set_option("ao", "null")?;
            }
            Ok(())
        })
        .map_err(|e| anyhow!("mpv init failed: {e:?}"))?;

        for (name, fmt, id) in [
            ("time-pos", Format::Double, P_TIME),
            ("duration", Format::Double, P_DURATION),
            ("pause", Format::Flag, P_PAUSE),
            ("eof-reached", Format::Flag, P_EOF),
            ("paused-for-cache", Format::Flag, P_CACHE),
            ("video-codec", Format::String, P_CODEC),
            ("video-params/w", Format::Int64, P_W),
            ("video-params/h", Format::Int64, P_H),
            ("hwdec-current", Format::String, P_HWDEC),
            ("dwidth", Format::Int64, P_DW),
            ("dheight", Format::Int64, P_DH),
            ("mute", Format::Flag, P_MUTE),
            ("volume", Format::Double, P_VOLUME),
            ("track-list/count", Format::Int64, P_TRACKS),
            ("aid", Format::String, P_AID),
            ("sid", Format::String, P_SID),
        ] {
            mpv.observe_property(name, fmt, id)
                .map_err(|e| anyhow!("observe {name}: {e:?}"))?;
        }

        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            frames_rendered: AtomicU64::new(0),
            frames_before_current_file: AtomicU64::new(0),
            dsize: AtomicU64::new(0),
            status: Mutex::new(Status::default()),
            tracks: Mutex::new(Vec::new()),
            pending_subtitles: Mutex::new(None),
            dirty: Mutex::new(false),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            on_frame: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            started: Instant::now(),
        });
        shared.log_event(format!(
            "libmpv loaded: client API {maj}.{min}, {}",
            mpv.get_property::<String>("mpv-version").unwrap_or_default()
        ));

        // Build Self first so Drop cleans up whatever got started on any error.
        let mut this = Self {
            mpv: Arc::new(mpv),
            shared,
            render_thread: None,
            event_thread: None,
        };

        // Render context must exist before a file is loaded (vo=libmpv).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        this.render_thread = Some({
            let mpv = this.mpv.clone();
            let shared = this.shared.clone();
            std::thread::Builder::new()
                .name("mpv-render".into())
                .spawn(move || render_loop(mpv, shared, ready_tx))?
        });
        ready_rx
            .recv()
            .map_err(|e| anyhow!("render thread died during init: {e}"))??;

        this.event_thread = Some({
            let mpv = this.mpv.clone();
            let shared = this.shared.clone();
            std::thread::Builder::new()
                .name("mpv-events".into())
                .spawn(move || event_loop(mpv, shared))?
        });

        Ok(this)
    }

    /// Called on the render thread every time a new frame is published.
    /// The callback must be non-blocking and must not call into `Player`.
    pub fn set_frame_callback(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.shared.on_frame.lock() = Some(Arc::new(f));
    }

    /// Load a URL, optionally starting at `start_secs`, with optional HTTP
    /// headers (e.g. auth). Both are *per-file* options, so neither leaks into
    /// later loads, and the header is not set globally on the mpv handle.
    pub fn load(&self, url: &str, start_secs: f64, headers: &[String]) -> Result<()> {
        self.load_with_subtitles(url, start_secs, headers, Vec::new(), None)
    }

    /// Like [`Self::load`], plus external subtitles (fetched with the same
    /// per-file HTTP headers) and the subtitle preference to apply once the
    /// file's tracks are known.
    pub fn load_with_subtitles(
        &self,
        url: &str,
        start_secs: f64,
        headers: &[String],
        external: Vec<ExternalSubtitle>,
        preference: Option<SubtitlePreference>,
    ) -> Result<()> {
        *self.shared.pending_subtitles.lock() = Some(PendingSubtitles { external, preference });
        let mut opts = vec![];
        if start_secs > 1.0 {
            opts.push(format!("start={start_secs:.3}"));
        }
        if !headers.is_empty() {
            if headers.iter().any(|h| h.contains(',')) {
                bail!("HTTP header values must not contain ','");
            }
            // %len% quoting lets the value contain '=', ':', '"' and spaces.
            let v = headers.join(",");
            opts.push(format!("http-header-fields=%{}%{v}", v.len()));
        }
        let per_file = opts.join(",");
        self.shared.frames_before_current_file.store(u64::MAX, Ordering::Release);
        // mpv >= 0.38: loadfile <url> <flags> <index> <options>
        self.mpv
            .command("loadfile", &[url, "replace", "-1", &per_file])
            .map_err(|e| anyhow!("loadfile: {e:?}"))?;
        self.set("pause", false);
        Ok(())
    }

    /// Fire-and-forget command: a failure is logged rather than returned,
    /// because callers (key presses, UI clicks) have nothing to recover.
    fn run(&self, name: &str, args: &[&str]) {
        if let Err(e) = self.mpv.command(name, args) {
            self.shared.log_event(format!("{name} {args:?} failed: {e:?}"));
        }
    }

    /// Like [`Self::run`], for setting a property.
    fn set<T: SetData>(&self, name: &str, value: T) {
        if let Err(e) = self.mpv.set_property(name, value) {
            self.shared.log_event(format!("set {name} failed: {e:?}"));
        }
    }

    /// The newest rendered frame, if any.
    pub fn latest_frame(&self) -> Option<Arc<VideoFrame>> {
        self.shared.latest.lock().clone()
    }

    /// Number of frames rendered since creation.
    pub fn frames_rendered(&self) -> u64 {
        self.shared.frames_rendered.load(Ordering::Relaxed)
    }

    /// Frames rendered from the file loaded last; 0 until it has loaded.
    pub fn frames_of_current_file(&self) -> u64 {
        match self.shared.frames_before_current_file.load(Ordering::Acquire) {
            u64::MAX => 0,
            before => self.frames_rendered().saturating_sub(before),
        }
    }

    /// Timestamped log of notable mpv events.
    pub fn events(&self) -> Vec<String> {
        self.shared.events.lock().clone()
    }

    /// Cached, non-blocking snapshot (updated by property observers).
    pub fn status(&self) -> Status {
        self.shared.status.lock().clone()
    }

    /// Read an mpv property as a string (empty if unavailable).
    pub fn property_string(&self, name: &str) -> String {
        self.mpv.get_property::<String>(name).unwrap_or_default()
    }

    /// Toggle between playing and paused.
    pub fn toggle_pause(&self) {
        self.run("cycle", &["pause"]);
    }

    /// Pause or resume.
    pub fn set_paused(&self, paused: bool) {
        self.set("pause", paused);
    }

    /// Seek by `secs` (negative = backwards).
    pub fn seek_relative(&self, secs: f64) {
        self.run("seek", &[&format!("{secs}"), "relative"]);
    }

    /// Seek to `secs` from the start.
    pub fn seek_absolute(&self, secs: f64) {
        self.run("seek", &[&format!("{secs:.3}"), "absolute"]);
    }

    /// Seek to `pct` percent of the duration.
    pub fn seek_percent(&self, pct: f64) {
        self.run("seek", &[&format!("{pct:.3}"), "absolute-percent"]);
    }

    /// Cached track list (rebuilt on the event thread when tracks/aid/sid change).
    pub fn tracks(&self) -> Vec<Track> {
        self.shared.tracks.lock().clone()
    }
}

fn read_tracks(mpv: &Mpv) -> Vec<Track> {
    let count: i64 = mpv.get_property("track-list/count").unwrap_or(0);
    (0..count)
        .map(|i| {
            let string = |key: &str| {
                mpv.get_property::<String>(&format!("track-list/{i}/{key}"))
                    .unwrap_or_default()
            };
            let (language, title, codec) = (string("lang"), string("title"), string("codec"));
            let label = if title.is_empty() {
                [language.as_str(), codec.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ")
            } else {
                title.clone()
            };
            Track {
                id: mpv.get_property(&format!("track-list/{i}/id")).unwrap_or(0),
                kind: string("type"),
                language,
                title,
                label,
                selected: mpv
                    .get_property(&format!("track-list/{i}/selected"))
                    .unwrap_or(false),
            }
        })
        .collect()
}

/// Runs on the event thread: `sub-add` blocks while the file is fetched.
fn apply_pending_subtitles(mpv: &Mpv, shared: &Shared) {
    let Some(pending) = shared.pending_subtitles.lock().take() else {
        return;
    };
    for subtitle in &pending.external {
        // "auto" adds without selecting; the preference decides below.
        match mpv.command(
            "sub-add",
            &[&subtitle.url, "auto", &subtitle.title, &subtitle.language],
        ) {
            Ok(()) => shared.log_event(format!("sub-add: {}", subtitle.title)),
            Err(e) => shared.log_event(format!("sub-add {} failed: {e:?}", subtitle.title)),
        }
    }
    let tracks = read_tracks(mpv);
    if let Some(preference) = &pending.preference
        && let Some(choice) = choose_subtitle(&tracks, preference)
    {
        let result = match choice {
            Some(id) => mpv.set_property("sid", id),
            None => mpv.set_property("sid", "no"),
        };
        if let Err(e) = result {
            shared.log_event(format!("restoring subtitle preference failed: {e:?}"));
        }
    }
    *shared.tracks.lock() = read_tracks(mpv);
}

impl Player {
    /// Cycle "aid" or "sid".
    pub fn cycle(&self, prop: &str) {
        self.run("cycle", &[prop]);
    }

    /// Select a subtitle track by id, or turn subtitles off with `None`.
    pub fn select_subtitle(&self, id: Option<i64>) -> Result<()> {
        match id {
            Some(id) => self.mpv.set_property("sid", id),
            None => self.mpv.set_property("sid", "no"),
        }
        .map_err(|e| anyhow!("select subtitle: {e:?}"))
    }

    /// Set the volume in percent (clamped to 0–130).
    pub fn set_volume(&self, v: i64) {
        self.set("volume", v.clamp(0, 130));
    }

    /// Cached (observed) volume.
    pub fn volume(&self) -> i64 {
        self.shared.status.lock().volume.round() as i64
    }

    /// Toggle mute.
    pub fn toggle_mute(&self) {
        self.run("cycle", &["mute"]);
    }

    /// Cached (observed) mute state.
    pub fn is_muted(&self) -> bool {
        self.shared.status.lock().muted
    }

    /// Stop playback and unload the file.
    pub fn stop(&self) {
        self.run("stop", &[]);
    }
}

impl Drop for Player {
    #[expect(unsafe_code, reason = "libmpv2 has no safe wrapper for mpv_wakeup")]
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.run("stop", &[]);
        {
            *self.shared.dirty.lock() = true;
            let _woken: usize = self.shared.cv.notify_all();
        }
        // SAFETY: `self.mpv` keeps the handle alive, and mpv_wakeup is thread-safe.
        unsafe { sys::mpv_wakeup(self.mpv.ctx.as_ptr()) };
        for (name, thread) in [("render", self.render_thread.take()), ("event", self.event_thread.take())] {
            if let Some(thread) = thread
                && thread.join().is_err()
            {
                log::error!("mpv {name} thread panicked");
            }
        }
        // The last Arc<Mpv> drops with `self` -> libmpv2 calls mpv_destroy,
        // which shuts the core down since no other handles exist.
    }
}

// ---------------------------------------------------------------------------
// Event thread: the only place that observes core state.
// ---------------------------------------------------------------------------

fn event_loop(mpv: Arc<Mpv>, shared: Arc<Shared>) {
    let (mut dw, mut dh) = (0i64, 0i64);
    while !shared.stop.load(Ordering::SeqCst) {
        let Some(ev) = mpv.wait_event(0.25) else {
            continue;
        };
        match ev {
            Ok(Event::PropertyChange {
                change,
                reply_userdata,
                ..
            }) => {
                let mut st = shared.status.lock();
                match (reply_userdata, change) {
                    (P_TIME, PropertyData::Double(v)) => st.position = v,
                    (P_DURATION, PropertyData::Double(v)) => st.duration = v,
                    (P_PAUSE, PropertyData::Flag(v)) => st.paused = v,
                    (P_EOF, PropertyData::Flag(v)) => st.eof = v,
                    (P_CACHE, PropertyData::Flag(v)) => st.buffering = v,
                    (P_MUTE, PropertyData::Flag(v)) => st.muted = v,
                    (P_VOLUME, PropertyData::Double(v)) => st.volume = v,
                    (P_TIME, PropertyData::None) => st.position = 0.0,
                    (P_DURATION, PropertyData::None) => st.duration = 0.0,
                    (P_CODEC, PropertyData::None) => st.video_codec.clear(),
                    (P_CODEC, PropertyData::Str(v)) => st.video_codec = v.to_string(),
                    (P_W, PropertyData::Int64(v)) => st.width = v,
                    (P_H, PropertyData::Int64(v)) => st.height = v,
                    (P_HWDEC, PropertyData::Str(v)) => st.hwdec = v.to_string(),
                    (P_DW, PropertyData::Int64(v)) => dw = v,
                    (P_DH, PropertyData::Int64(v)) => dh = v,
                    (P_DW, PropertyData::None) => dw = 0,
                    (P_DH, PropertyData::None) => dh = 0,
                    _ => {}
                }
                drop(st);
                if matches!(reply_userdata, P_TRACKS | P_AID | P_SID) {
                    // Event thread may block on the core; the UI never does.
                    *shared.tracks.lock() = read_tracks(&mpv);
                }
                if matches!(reply_userdata, P_DW | P_DH) {
                    let (width, height) = (u64::try_from(dw).unwrap_or(0), u64::try_from(dh).unwrap_or(0));
                    let packed = (width << 32) | (height & 0xffff_ffff);
                    shared.dsize.store(packed, Ordering::Release);
                }
            }
            Ok(Event::StartFile) => shared.log_event("start-file".into()),
            Ok(Event::FileLoaded) => {
                let w: i64 = mpv.get_property("width").unwrap_or(0);
                let h: i64 = mpv.get_property("height").unwrap_or(0);
                let codec: String = mpv.get_property("video-codec").unwrap_or_default();
                let dur: f64 = mpv.get_property("duration").unwrap_or(0.0);
                shared.log_event(format!("file-loaded: {w}x{h} codec={codec} duration={dur:.1}s"));
                let frames = shared.frames_rendered.load(Ordering::Relaxed);
                shared.frames_before_current_file.store(frames, Ordering::Release);
                apply_pending_subtitles(&mpv, &shared);
            }
            Ok(Event::VideoReconfig) => {
                let hw: String = mpv.get_property("hwdec-current").unwrap_or_default();
                shared.log_event(format!("video-reconfig: display {dw}x{dh} hwdec={hw}"));
            }
            Ok(Event::PlaybackRestart) => {
                let pos: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
                shared.log_event(format!("playback-restart at {pos:.2}s"));
            }
            Ok(Event::EndFile(reason)) => shared.log_event(format!("end-file reason={reason}")),
            Ok(Event::Shutdown) => break,
            Ok(_) => {}
            Err(e) => shared.log_event(format!("event error: {e:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Render thread: owns the GL context and calls ONLY mpv_render_* functions.
// ---------------------------------------------------------------------------

#[expect(unsafe_code, reason = "C callback for the mpv render API")]
unsafe extern "C" fn on_mpv_update(ctx: *mut c_void) {
    // Called from arbitrary mpv threads; must not call mpv APIs here.
    // SAFETY: `ctx` is the `Arc<Shared>` pointer registered in render_loop, which
    // holds that Arc until it unregisters this callback.
    let shared = unsafe { &*(ctx as *const Shared) };
    *shared.dirty.lock() = true;
    let _woken: bool = shared.cv.notify_one();
}

#[expect(unsafe_code, reason = "reads mpv's static error strings")]
fn mpv_error_message(code: i32) -> String {
    // SAFETY: accepts any error code and returns a pointer to a static string.
    let message = unsafe { sys::mpv_error_string(code) };
    // SAFETY: mpv_error_string never returns null, and the string is NUL-terminated and static.
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

/// Display size scaled down (keeping the aspect ratio) to fit `MAX_W` x `MAX_H`,
/// rounded down to even dimensions of at least 2.
fn render_size(display_width: u32, display_height: u32) -> (u32, u32) {
    let (width, height) = (u64::from(display_width), u64::from(display_height));
    let (max_width, max_height) = (u64::from(MAX_W), u64::from(MAX_H));
    let (width, height) = if width <= max_width && height <= max_height {
        (width, height)
    } else if width * max_height >= height * max_width {
        (max_width, height * max_width / width.max(1))
    } else {
        (width * max_height / height.max(1), max_height)
    };
    let even = |value: u64, max: u32| u32::try_from(value).unwrap_or(max).max(2) & !1;
    (even(width, MAX_W), even(height, MAX_H))
}

struct RenderCtx(*mut sys::mpv_render_context);

impl Drop for RenderCtx {
    #[expect(unsafe_code, reason = "frees the mpv render context")]
    fn drop(&mut self) {
        // SAFETY: created by mpv_render_context_create, and freed only here, while
        // the GL context it was created on is still current.
        unsafe { sys::mpv_render_context_free(self.0) };
    }
}

#[expect(unsafe_code, reason = "calls the mpv OpenGL render API")]
fn gl_render(rctx: &RenderCtx, framebuffer: u32, width: u32, height: u32) -> i32 {
    let (Ok(fbo), Ok(w), Ok(h)) = (c_int::try_from(framebuffer), c_int::try_from(width), c_int::try_from(height)) else {
        return sys::mpv_error_MPV_ERROR_INVALID_PARAMETER;
    };
    let mut target = sys::mpv_opengl_fbo { fbo, w, h, internal_format: gl::RGBA8 as c_int };
    // Unflipped: row 0 of the texture is the top of the picture, matching the
    // top-down row order of the IOSurface it is converted into.
    let mut flip_y: c_int = 0;
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
            data: (&raw mut target).cast::<c_void>(),
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
            data: (&raw mut flip_y).cast::<c_void>(),
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    // SAFETY: `rctx` is live and its GL context is current on this thread; every
    // param points at locals that outlive this synchronous call.
    unsafe { sys::mpv_render_context_render(rctx.0, params.as_mut_ptr()) }
}

/// Logs a render error once rather than for every frame, so a persistent
/// failure cannot grow the event log without bound.
#[derive(Default)]
struct ErrorOnce(Option<String>);

impl ErrorOnce {
    fn report(&mut self, shared: &Shared, message: String) {
        if self.0.as_ref() != Some(&message) {
            shared.log_event(message.clone());
            self.0 = Some(message);
        }
    }
}

#[expect(unsafe_code, reason = "owns the mpv render context (C API)")]
fn render_loop(mpv: Arc<Mpv>, shared: Arc<Shared>, ready: std::sync::mpsc::Sender<Result<()>>) {
    let fail = |error: anyhow::Error| {
        let message = format!("{error:#}");
        if ready.send(Err(error)).is_err() {
            log::error!("render thread setup failed after Player::new gave up: {message}");
        }
    };
    // Declared before `rctx`, so they drop after it: mpv frees its GL objects
    // while the context is still current.
    let (gl_context, mut converter) = match GlContext::new_current()
        .and_then(|context| Nv12Converter::new(&context).map(|converter| (context, converter)))
    {
        Ok(setup) => setup,
        Err(e) => return fail(e.context("OpenGL setup for video failed")),
    };
    let mut init = sys::mpv_opengl_init_params {
        get_proc_address: Some(crate::gpu_video::mpv_get_proc_address),
        get_proc_address_ctx: std::ptr::null_mut(),
    };
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
            data: c"opengl".as_ptr() as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
            data: (&raw mut init).cast::<c_void>(),
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    let mut raw: *mut sys::mpv_render_context = std::ptr::null_mut();
    // SAFETY: `mpv` is a live handle, `raw` is a valid out-pointer, `params` is
    // terminated by MPV_RENDER_PARAM_INVALID, and the GL context is current.
    let rc = unsafe { sys::mpv_render_context_create(&mut raw, mpv.ctx.as_ptr(), params.as_mut_ptr()) };
    // From here on the render thread never touches `mpv` (client API) again.
    drop(mpv);
    if rc < 0 || raw.is_null() {
        return fail(anyhow!("mpv_render_context_create(opengl) failed: {}", mpv_error_message(rc)));
    }
    let rctx = RenderCtx(raw);
    // SAFETY: `shared` (an Arc held until the callback is cleared below) outlives
    // every call mpv makes to `on_mpv_update`.
    unsafe {
        sys::mpv_render_context_set_update_callback(rctx.0, Some(on_mpv_update), Arc::as_ptr(&shared) as *mut c_void);
    }
    shared.log_event("OpenGL render context created (frames -> NV12 IOSurface -> Metal)".into());
    if ready.send(Ok(())).is_err() {
        // Player::new is gone, so nobody will ever stop this thread.
        return;
    }

    let mut errors = ErrorOnce::default();
    while !shared.stop.load(Ordering::SeqCst) {
        {
            let mut dirty = shared.dirty.lock();
            if !*dirty {
                let _timed_out = shared.cv.wait_for(&mut dirty, Duration::from_millis(100));
            }
            *dirty = false;
        }
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        // SAFETY: `rctx` is live, and only this thread uses it.
        let flags = unsafe { sys::mpv_render_context_update(rctx.0) };
        if flags & (sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64) == 0 {
            continue;
        }

        let packed = shared.dsize.load(Ordering::Acquire);
        let (dw, dh) = ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32);
        // Size not known yet (or audio-only): still consume the frame so the VO
        // never waits on us, into a tiny target that is not published.
        let publish = dw != 0 && dh != 0;
        let (w, h) = if publish { render_size(dw, dh) } else { (16, 16) };
        let framebuffer = match converter.target(w, h) {
            Ok(framebuffer) => framebuffer,
            Err(e) => {
                errors.report(&shared, format!("render target {w}x{h}: {e:#}"));
                continue;
            }
        };
        let rc = gl_render(&rctx, framebuffer, w, h);
        if rc < 0 {
            errors.report(&shared, format!("render error: {}", mpv_error_message(rc)));
            continue;
        }
        if !publish {
            continue;
        }
        let pixel_buffer = match converter.convert() {
            Ok(pixel_buffer) => pixel_buffer,
            Err(e) => {
                errors.report(&shared, format!("NV12 conversion: {e:#}"));
                continue;
            }
        };

        let seq = shared.frames_rendered.fetch_add(1, Ordering::Relaxed) + 1;
        *shared.latest.lock() = Some(Arc::new(VideoFrame {
            width: w,
            height: h,
            pixel_buffer,
            seq,
        }));
        // Clone the callback out of the lock before invoking it.
        let cb = shared.on_frame.lock().clone();
        if let Some(cb) = cb {
            cb();
        }
    }
    // Detach callback before freeing, so mpv never calls into a dead Shared.
    // SAFETY: `rctx` is still live; clearing the callback has no other preconditions.
    unsafe {
        sys::mpv_render_context_set_update_callback(rctx.0, None, std::ptr::null_mut());
    }
    drop(rctx);
    drop(converter);
    drop(gl_context);
}

/// CPU view of a frame's NV12 planes (proof artifacts and tests only).
struct Nv12Planes<'a> {
    luma: &'a [u8],
    luma_stride: usize,
    chroma: &'a [u8],
    chroma_stride: usize,
}

impl Nv12Planes<'_> {
    /// `(y, cb, cr)` at pixel `(x, y)`; out-of-range reads give black.
    fn sample(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let luma = self.luma.get(y * self.luma_stride + x).copied().unwrap_or(0);
        let chroma = (y / 2) * self.chroma_stride + (x / 2) * 2;
        let cb = self.chroma.get(chroma).copied().unwrap_or(128);
        let cr = self.chroma.get(chroma + 1).copied().unwrap_or(128);
        (luma, cb, cr)
    }
}

impl VideoFrame {
    #[expect(unsafe_code, reason = "reads the locked CVPixelBuffer planes")]
    fn read_planes<R>(&self, read: impl FnOnce(&Nv12Planes<'_>) -> R) -> Result<R> {
        let buffer = &self.pixel_buffer;
        let locked = buffer.lock_base_address(kCVPixelBufferLock_ReadOnly);
        if locked != 0 {
            bail!("CVPixelBufferLockBaseAddress failed ({locked})");
        }
        let plane = |index: usize| -> &[u8] {
            // SAFETY: the buffer is locked until after `read` returns, so the plane's
            // base address stays valid for `bytes_per_row * height` bytes.
            let base = unsafe { buffer.get_base_address_of_plane(index) };
            if base.is_null() {
                return &[];
            }
            let length = buffer.get_bytes_per_row_of_plane(index) * buffer.get_height_of_plane(index);
            // SAFETY: see above; the slice is only read.
            unsafe { std::slice::from_raw_parts(base.cast::<u8>(), length) }
        };
        let planes = Nv12Planes {
            luma: plane(0),
            luma_stride: buffer.get_bytes_per_row_of_plane(0),
            chroma: plane(1),
            chroma_stride: buffer.get_bytes_per_row_of_plane(1),
        };
        let result = read(&planes);
        let unlocked = buffer.unlock_base_address(kCVPixelBufferLock_ReadOnly);
        if unlocked != 0 {
            log::warn!("CVPixelBufferUnlockBaseAddress failed ({unlocked})");
        }
        Ok(result)
    }

    /// Save as PNG for proof artifacts, using the same BT.601 full-range
    /// conversion as GPUI's surface shader.
    pub fn save_png(&self, path: &std::path::Path) -> Result<()> {
        let (width, height) = (self.width as usize, self.height as usize);
        let rgba = self.read_planes(|planes| {
            let mut rgba = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                for x in 0..width {
                    let (luma, cb, cr) = planes.sample(x, y);
                    let (luma, cb, cr) = (f32::from(luma), f32::from(cb) - 128.0, f32::from(cr) - 128.0);
                    let channel = |value: f32| value.round().clamp(0.0, 255.0) as u8;
                    rgba.extend_from_slice(&[
                        channel(luma + 1.402 * cr),
                        channel(luma - 0.3441 * cb - 0.7141 * cr),
                        channel(luma + 1.772 * cb),
                        255,
                    ]);
                }
            }
            rgba
        })?;
        let img = image::RgbaImage::from_raw(self.width, self.height, rgba).ok_or_else(|| anyhow!("bad frame buffer"))?;
        img.save(path)?;
        Ok(())
    }

    /// Luma variance — a solid/black frame has ~0.
    pub fn luma_variance(&self) -> Result<f64> {
        let (width, height) = (self.width as usize, self.height as usize);
        self.read_planes(|planes| {
            let (mut n, mut sum, mut sq) = (0f64, 0f64, 0f64);
            for y in (0..height).step_by(7) {
                for x in (0..width).step_by(7) {
                    let luma = f64::from(planes.sample(x, y).0);
                    n += 1.0;
                    sum += luma;
                    sq += luma * luma;
                }
            }
            if n == 0.0 {
                return 0.0;
            }
            let mean = sum / n;
            sq / n - mean * mean
        })
    }

    /// Cheap content fingerprint (FNV-1a over a pixel sample).
    pub fn fingerprint(&self) -> Result<u64> {
        let (width, height) = (self.width as usize, self.height as usize);
        self.read_planes(|planes| {
            let mut h: u64 = 0xcbf29ce484222325;
            for index in (0..width * height).step_by(97) {
                let (luma, cb, cr) = planes.sample(index % width.max(1), index / width.max(1));
                for byte in [luma, cb, cr] {
                    h ^= u64::from(byte);
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            h
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtitle(id: i64, language: &str, title: &str) -> Track {
        Track {
            id,
            kind: "sub".into(),
            language: language.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn preference(language: &str, title: &str) -> SubtitlePreference {
        SubtitlePreference::Track { language: language.into(), title: title.into() }
    }

    #[test]
    fn choose_subtitle_matches_preference() {
        let tracks = [
            Track { id: 1, kind: "audio".into(), language: "vie".into(), ..Default::default() },
            subtitle(1, "eng", "English - Hearing Impaired - SUBRIP - External"),
            subtitle(2, "vie", "Vietnamese - SUBRIP - External"),
            subtitle(3, "eng", "English - SUBRIP - External"),
        ];
        assert_eq!(choose_subtitle(&tracks, &SubtitlePreference::Off), Some(None));
        assert_eq!(choose_subtitle(&tracks, &preference("vie", "Vietnamese - SUBRIP - External")), Some(Some(2)));
        assert_eq!(choose_subtitle(&tracks, &preference("eng", "English - SUBRIP - External")), Some(Some(3)));
        // Next episode lacks the exact title: fall back to the same language.
        assert_eq!(choose_subtitle(&tracks, &preference("vie", "Vietnamese - ASS - External")), Some(Some(2)));
        assert_eq!(choose_subtitle(&tracks, &preference("fre", "French")), None);
        assert_eq!(choose_subtitle(&tracks, &preference("", "Commentary")), None);
    }
    #[test]
    fn render_size_fits_1080p_and_stays_even() {
        assert_eq!(render_size(1920, 1080), (1920, 1080));
        assert_eq!(render_size(1280, 720), (1280, 720));
        assert_eq!(render_size(3840, 2160), (1920, 1080));
        assert_eq!(render_size(3840, 1600), (1920, 800));
        assert_eq!(render_size(1440, 2160), (720, 1080));
        assert_eq!(render_size(721, 481), (720, 480));
        assert_eq!(render_size(1, 1), (2, 2));
    }
}
