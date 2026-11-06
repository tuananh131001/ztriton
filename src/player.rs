//! mpv playback core.
//!
//! * `libmpv2::Mpv` drives playback (options, commands, events).
//! * Player state is *observed* (`observe_property`) on the event thread and
//!   cached, so UI reads never block on the mpv core lock.
//! * Video frames come from mpv's *software* render API
//!   (`MPV_RENDER_API_TYPE_SW`) on a dedicated render thread that only calls
//!   `mpv_render_*` functions (as required by render.h's threading rules).
//!   Frames are converted to tightly packed BGRA and published as
//!   [`VideoFrame`]s that the UI uploads to the GPU through GPUI.

use anyhow::{Result, anyhow, bail};
use libmpv2::{
    Format, Mpv, SetData,
    events::{Event, PropertyData},
};
use libmpv2_sys as sys;
use parking_lot::{Condvar, Mutex};
use std::ffi::{CStr, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Largest frame we ask the software renderer for. 4K sources are scaled
/// down by mpv to keep the CPU path real-time.
const MAX_W: u32 = 1920;
const MAX_H: u32 = 1080;

/// One decoded, display-ready frame, tightly packed BGRA (alpha = 255).
pub struct VideoFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, row-major, no padding.
    pub bgra: Vec<u8>,
    /// Increases by one for every rendered frame.
    pub seq: u64,
}

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
            init.set_option("hwdec", "auto-copy")?;
            init.set_option("keep-open", "yes")?;
            init.set_option("idle", "yes")?;
            init.set_option("terminal", "no")?;
            init.set_option("config", "no")?;
            init.set_option("input-default-bindings", "no")?;
            init.set_option("osc", "no")?;
            init.set_option("ytdl", "no")?;
            init.set_option("cache", "yes")?;
            init.set_option("demuxer-max-bytes", "150MiB")?;
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

    /// Take the newest frame if it is newer than `after_seq` (UI path: avoids an
    /// 8 MB clone per frame when the UI is the only holder).
    pub fn take_frame_after(&self, after_seq: u64) -> Option<Arc<VideoFrame>> {
        let mut slot = self.shared.latest.lock();
        match slot.as_ref() {
            Some(f) if f.seq > after_seq => slot.take(),
            _ => None,
        }
    }

    /// Number of frames rendered since creation.
    pub fn frames_rendered(&self) -> u64 {
        self.shared.frames_rendered.load(Ordering::Relaxed)
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
// Software render thread: calls ONLY mpv_render_* functions.
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
        // SAFETY: created by mpv_render_context_create, and freed only here.
        unsafe { sys::mpv_render_context_free(self.0) };
    }
}

/// 64-byte aligned scratch buffer (mpv recommends 64-byte aligned SW targets).
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Chunk([u8; 64]);

#[expect(unsafe_code, reason = "calls the mpv software render API")]
fn sw_render(rctx: &RenderCtx, fmt: &CStr, w: u32, h: u32, stride: usize, ptr: *mut c_void) -> i32 {
    let mut size = [w as i32, h as i32];
    let mut stride_sz: usize = stride;
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
            data: size.as_mut_ptr() as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
            data: fmt.as_ptr() as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
            data: &mut stride_sz as *mut usize as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
            data: ptr,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    // SAFETY: `rctx` is live, and every param points at locals (or the caller's
    // buffer of `stride * h` bytes) that outlive this synchronous call.
    unsafe { sys::mpv_render_context_render(rctx.0, params.as_mut_ptr()) }
}

#[expect(unsafe_code, reason = "owns the mpv render context (C API)")]
fn render_loop(mpv: Arc<Mpv>, shared: Arc<Shared>, ready: std::sync::mpsc::Sender<Result<()>>) {
    let api_sw = c"sw";
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
            data: api_sw.as_ptr() as *mut c_void,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    let mut raw: *mut sys::mpv_render_context = std::ptr::null_mut();
    // SAFETY: `mpv` is a live handle, `raw` is a valid out-pointer, and `params`
    // is terminated by MPV_RENDER_PARAM_INVALID.
    let rc = unsafe { sys::mpv_render_context_create(&mut raw, mpv.ctx.as_ptr(), params.as_mut_ptr()) };
    // From here on the render thread never touches `mpv` (client API) again.
    drop(mpv);
    if rc < 0 || raw.is_null() {
        let msg = mpv_error_message(rc);
        if ready.send(Err(anyhow!("mpv_render_context_create(sw) failed: {msg}"))).is_err() {
            log::error!("render context creation failed after Player::new gave up: {msg}");
        }
        return;
    }
    let rctx = RenderCtx(raw);
    // SAFETY: `shared` (an Arc held until the callback is cleared below) outlives
    // every call mpv makes to `on_mpv_update`.
    unsafe {
        sys::mpv_render_context_set_update_callback(rctx.0, Some(on_mpv_update), Arc::as_ptr(&shared) as *mut c_void);
    }
    shared.log_event("sw render context created (MPV_RENDER_API_TYPE_SW)".into());
    if ready.send(Ok(())).is_err() {
        // Player::new is gone, so nobody will ever stop this thread.
        return;
    }

    let fmt = c"bgr0";
    let mut scratch: Vec<Chunk> = Vec::new();
    let mut tiny = [Chunk([0; 64]); 4];

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
        if dw == 0 || dh == 0 {
            // Size not known yet (or audio-only): still consume the frame so the
            // VO never waits on us, into a throwaway 16x4 target.
            let rc = sw_render(&rctx, fmt, 16, 4, 64, tiny.as_mut_ptr() as *mut c_void);
            if rc < 0 {
                shared.log_event(format!("sw render (discard) error {rc}"));
            }
            continue;
        }
        let (w, h) = render_size(dw, dh);

        let row = w as usize * 4;
        let stride = (row + 63) & !63;
        let need_chunks = (stride * h as usize).div_ceil(64);
        if scratch.len() < need_chunks {
            scratch.resize(need_chunks, Chunk([0; 64]));
        }
        let rc = sw_render(&rctx, fmt, w, h, stride, scratch.as_mut_ptr() as *mut c_void);
        if rc < 0 {
            let msg = mpv_error_message(rc);
            shared.log_event(format!("sw render error: {msg}"));
            continue;
        }

        // Pack rows (no-op layout when stride == row) and force opaque alpha.
        // SAFETY: `scratch` holds at least `need_chunks * 64 >= stride * h` initialised
        // bytes, and is not mutated while `src` is alive.
        let src = unsafe { std::slice::from_raw_parts(scratch.as_ptr() as *const u8, stride * h as usize) };
        let mut bgra = Vec::with_capacity(row * h as usize);
        for line in src.chunks_exact(stride) {
            bgra.extend_from_slice(line.get(..row).unwrap_or_default());
        }
        for px in bgra.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }

        let seq = shared.frames_rendered.fetch_add(1, Ordering::Relaxed) + 1;
        *shared.latest.lock() = Some(Arc::new(VideoFrame {
            width: w,
            height: h,
            bgra,
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
}

impl VideoFrame {
    /// Save as PNG (BGRA -> RGBA) for proof artifacts.
    pub fn save_png(&self, path: &std::path::Path) -> Result<()> {
        let mut rgba = self.bgra.clone();
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(self.width, self.height, rgba)
            .ok_or_else(|| anyhow!("bad frame buffer"))?;
        img.save(path)?;
        Ok(())
    }

    /// Luma variance — a solid/black frame has ~0.
    pub fn luma_variance(&self) -> f64 {
        let (mut n, mut sum, mut sq) = (0f64, 0f64, 0f64);
        for px in self.bgra.as_chunks::<4>().0.iter().step_by(7) {
            let y = 0.114 * px[0] as f64 + 0.587 * px[1] as f64 + 0.299 * px[2] as f64;
            n += 1.0;
            sum += y;
            sq += y * y;
        }
        if n == 0.0 {
            return 0.0;
        }
        let mean = sum / n;
        sq / n - mean * mean
    }

    /// Cheap content fingerprint (FNV-1a over a pixel sample).
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in self.bgra.iter().step_by(97) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
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
