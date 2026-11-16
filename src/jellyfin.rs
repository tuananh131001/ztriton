//! Minimal blocking Jellyfin REST client (ureq). Call from background threads.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CLIENT: &str = "ztriton";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-user config dir: ~/Library/Application Support/ztriton
pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Application Support/ztriton"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Stable per-install device id (Jellyfin keys tokens/sessions by DeviceId).
/// Override with ZTRITON_DEVICE_ID (tests use a separate id).
pub fn device_id() -> String {
    if let Ok(id) = std::env::var("ZTRITON_DEVICE_ID") {
        return id;
    }
    let dir = config_dir();
    let path = dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut id = String::from("ztriton-");
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        id.push_str(&format!("{:016x}", h.finish()));
    }
    // Without a saved id every launch registers as a new device, but playback still works.
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &id)) {
        log::warn!("could not save device id to {}: {e}", path.display());
    }
    id
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// An authenticated connection to one Jellyfin server.
#[derive(Clone)]
pub struct Jellyfin {
    /// Server URL without a trailing `/`.
    pub base: String,
    token: String,
    /// Device id sent with every request.
    pub device_id: String,
    /// Id of the signed-in user.
    pub user_id: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Jellyfin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jellyfin")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

/// A library entry: movie, episode, series, folder or view.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    /// Jellyfin item id.
    pub id: String,
    /// Display name.
    pub name: String,
    #[serde(rename = "Type")]
    /// Item type, e.g. `"Movie"`, `"Episode"`, `"Series"`.
    pub kind: String,
    /// Library kind for views, e.g. `"movies"`, `"tvshows"`.
    #[serde(default)]
    pub collection_type: Option<String>,
    #[serde(default)]
    /// Plot summary.
    pub overview: Option<String>,
    #[serde(default)]
    /// Release year.
    pub production_year: Option<i32>,
    #[serde(default)]
    /// Runtime in Jellyfin ticks (100 ns).
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    /// Audience rating out of 10.
    pub community_rating: Option<f32>,
    #[serde(default)]
    /// Content rating, e.g. `"TV-PG"`.
    pub official_rating: Option<String>,
    #[serde(default)]
    /// Series name, for episodes.
    pub series_name: Option<String>,
    #[serde(default)]
    /// Series id, for episodes.
    pub series_id: Option<String>,
    #[serde(default)]
    /// Season number, for episodes.
    pub parent_index_number: Option<i32>,
    #[serde(default)]
    /// Episode number, for episodes.
    pub index_number: Option<i32>,
    #[serde(default)]
    /// Whether the item contains other items.
    pub is_folder: bool,
    #[serde(default)]
    /// Image type (e.g. `"Primary"`) to cache tag.
    pub image_tags: std::collections::HashMap<String, String>,
    #[serde(default)]
    /// Cache tags of the backdrop images.
    pub backdrop_image_tags: Vec<String>,
    #[serde(default)]
    /// Per-user playback state.
    pub user_data: Option<UserData>,
    #[serde(default)]
    /// Genre names.
    pub genres: Vec<String>,
}

/// The signed-in user's playback state for an item.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct UserData {
    #[serde(default)]
    /// Resume position in ticks.
    pub playback_position_ticks: i64,
    #[serde(default)]
    /// How much has been watched, in percent.
    pub played_percentage: Option<f64>,
    #[serde(default)]
    /// Whether the item is marked as watched.
    pub played: bool,
    #[serde(default)]
    /// When any device last reported playing the item (start, progress or
    /// stop), as a UTC timestamp.
    pub last_played_date: Option<String>,
}

impl Item {
    /// Whether the item is a video that can be played directly.
    pub fn is_playable(&self) -> bool {
        matches!(self.kind.as_str(), "Movie" | "Episode" | "Video" | "MusicVideo")
    }

    /// Runtime in seconds.
    pub fn runtime_secs(&self) -> Option<f64> {
        self.run_time_ticks.map(ticks_to_secs)
    }

    /// Where to resume playback, in seconds (0 if unwatched).
    pub fn resume_secs(&self) -> f64 {
        self.user_data
            .as_ref()
            .map(|u| ticks_to_secs(u.playback_position_ticks))
            .unwrap_or(0.0)
    }

    /// Secondary line for the UI: "Series · S1E9" for episodes, else the year.
    pub fn subtitle(&self) -> String {
        match (self.kind.as_str(), &self.series_name) {
            ("Episode", Some(series)) => format!(
                "{series} · S{}E{}",
                self.parent_index_number.unwrap_or(0),
                self.index_number.unwrap_or(0)
            ),
            _ => self
                .production_year
                .map(|y| y.to_string())
                .unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResponse {
    items: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemMediaSources {
    #[serde(default)]
    media_sources: Vec<MediaSource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaSource {
    id: String,
    #[serde(default)]
    media_streams: Vec<MediaStream>,
    #[serde(default)]
    supports_direct_play: bool,
    #[serde(default)]
    supports_direct_stream: bool,
    #[serde(default)]
    transcoding_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackInfoResponse {
    #[serde(default)]
    media_sources: Vec<MediaSource>,
    #[serde(default)]
    play_session_id: Option<String>,
}

/// How the server delivers a stream, as Jellyfin names it in playback reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayMethod {
    /// The file as-is.
    #[default]
    DirectPlay,
    /// The file's streams as-is, possibly in another container.
    DirectStream,
    /// Converted on the fly by the server.
    Transcode,
}

impl PlayMethod {
    /// Jellyfin's name for the method.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "DirectPlay",
            Self::DirectStream => "DirectStream",
            Self::Transcode => "Transcode",
        }
    }
}

/// Which server-side stream playback reports refer to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamIdentity {
    /// How the stream is delivered.
    pub method: PlayMethod,
    /// The media source (file version) being played.
    pub media_source_id: Option<String>,
    /// The server's id for this play, which also names its transcode job.
    pub play_session_id: Option<String>,
}

/// The stream the server chose for the device's playback settings.
#[derive(Clone, Debug)]
pub struct PlaybackStream {
    /// URL for mpv, without credentials (auth goes via [`Jellyfin::media_auth_header`]).
    pub url: String,
    /// What playback reports must name.
    pub identity: StreamIdentity,
    /// Why the server converts the file (e.g. `"VideoCodecNotSupported"`); empty for direct play.
    pub transcode_reasons: Vec<String>,
    /// Subtitles mpv must load separately from the stream.
    pub subtitles: Vec<crate::player::ExternalSubtitle>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaStream {
    #[serde(rename = "Type")]
    kind: String,
    index: i64,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    display_title: Option<String>,
    #[serde(default)]
    is_external: bool,
    #[serde(default)]
    supports_external_stream: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResponse {
    access_token: String,
    user: AuthUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthUser {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackReport<'a> {
    item_id: &'a str,
    position_ticks: i64,
    is_paused: bool,
    play_method: &'a str,
    can_seek: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_source_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    play_session_id: Option<&'a str>,
}

/// Seconds to Jellyfin ticks (100 ns).
pub fn secs_to_ticks(s: f64) -> i64 {
    (s * 10_000_000.0) as i64
}

/// Jellyfin ticks (100 ns) to seconds.
pub fn ticks_to_secs(t: i64) -> f64 {
    t as f64 / 10_000_000.0
}

fn auth_header(device_id: &str, token: Option<&str>) -> String {
    let mut h = format!(
        r#"MediaBrowser Client="{CLIENT}", Device="macOS", DeviceId="{device_id}", Version="{VERSION}""#
    );
    if let Some(t) = token {
        h.push_str(&format!(r#", Token="{t}""#));
    }
    h
}

impl Jellyfin {
    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(30))
            .build()
    }

    /// Normalise user input: trim, drop trailing '/', default to http://.
    pub fn normalize_server(base: &str) -> String {
        let b = base.trim().trim_end_matches('/');
        if b.contains("://") { b.to_string() } else { format!("http://{b}") }
    }

    /// Sign in with a username and password.
    pub fn login(base: &str, user: &str, password: &str) -> Result<Self> {
        let base = Self::normalize_server(base);
        let agent = Self::agent();
        let device_id = device_id();
        let resp: AuthResponse = agent
            .post(&format!("{base}/Users/AuthenticateByName"))
            .set("Authorization", &auth_header(&device_id, None))
            .send_json(serde_json::json!({ "Username": user, "Pw": password }))
            .map_err(|e| match e {
                ureq::Error::Status(401, _) => anyhow!("Incorrect username or password"),
                ureq::Error::Status(code, _) => anyhow!("Server rejected login (HTTP {code})"),
                ureq::Error::Transport(t) => anyhow!("Can't reach server: {t}"),
            })?
            .into_json()
            .map_err(|e| anyhow!("{base} did not answer like a Jellyfin server ({e})"))?;
        Ok(Self {
            base,
            token: resp.access_token,
            device_id,
            user_id: resp.user.id,
            agent,
        })
    }

    /// Rebuild a client from a saved token and check it is still valid.
    pub fn resume_session(base: &str, user_id: &str, token: &str) -> Result<Self> {
        let jf = Self {
            base: Self::normalize_server(base),
            token: token.to_string(),
            device_id: device_id(),
            user_id: user_id.to_string(),
            agent: Self::agent(),
        };
        jf.validate()?;
        Ok(jf)
    }

    /// GET /Users/Me — fails (401) once the token has been revoked.
    pub fn validate(&self) -> Result<()> {
        let me: AuthUser = self.get_json("/Users/Me")?;
        if me.id != self.user_id {
            return Err(anyhow!("saved session belongs to a different user"));
        }
        Ok(())
    }

    /// Revoke this token server-side (POST /Sessions/Logout).
    pub fn logout(&self) -> Result<()> {
        self.post_json("/Sessions/Logout", serde_json::json!({}))
    }

    /// What the Keychain needs to restore this session later.
    pub fn saved_session(&self) -> crate::keychain::SavedSession {
        crate::keychain::SavedSession { user_id: self.user_id.clone(), token: self.token.clone() }
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path_and_query: &str) -> Result<T> {
        let url = format!("{}{}", self.base, path_and_query);
        self.agent
            .get(&url)
            .set("Authorization", &self.auth())
            .call()
            .map_err(|e| anyhow!("GET {path_and_query}: {e}"))?
            .into_json()
            .map_err(Into::into)
    }

    fn post_for_json<T: for<'de> Deserialize<'de>>(&self, path_and_query: &str, body: serde_json::Value) -> Result<T> {
        self.agent
            .post(&format!("{}{}", self.base, path_and_query))
            .set("Authorization", &self.auth())
            .send_json(body)
            .map_err(|e| anyhow!("POST {path_and_query}: {e}"))?
            .into_json()
            .map_err(Into::into)
    }

    fn post_json(&self, path: &str, body: serde_json::Value) -> Result<()> {
        self.agent
            .post(&format!("{}{}", self.base, path))
            .set("Authorization", &self.auth())
            .send_json(body)
            .map(drop)
            .map_err(|e| anyhow!("POST {path}: {e}"))
    }

    fn auth(&self) -> String {
        auth_header(&self.device_id, Some(&self.token))
    }

    /// HTTP header that authorizes media requests (handed to mpv so the token
    /// never appears in a URL). Contains no ',' (mpv list separator).
    pub fn media_auth_header(&self) -> String {
        format!(r#"Authorization: MediaBrowser Token="{}""#, self.token)
    }

    const FIELDS: &'static str =
        "Overview,Genres,CommunityRating,OfficialRating,ProductionYear,ImageTags,BackdropImageTags";

    /// The user's libraries.
    pub fn views(&self) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!("/UserViews?userId={}", self.user_id))?;
        Ok(r.items)
    }

    /// Direct children of a library or folder, sorted by name.
    pub fn children(&self, parent_id: &str) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!(
            "/Items?userId={}&ParentId={}&SortBy=SortName&SortOrder=Ascending&Fields={}&Limit=500",
            self.user_id,
            enc(parent_id),
            Self::FIELDS
        ))?;
        Ok(r.items)
    }

    /// Most recently added items under `parent_id`.
    pub fn latest(&self, parent_id: &str, limit: usize) -> Result<Vec<Item>> {
        // /Items/Latest returns a bare array.
        self.get_json(&format!(
            "/Items/Latest?userId={}&ParentId={}&Limit={limit}&Fields={}",
            self.user_id,
            enc(parent_id),
            Self::FIELDS
        ))
    }

    /// Partially watched videos ("Continue Watching").
    pub fn resume(&self, limit: usize) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!(
            "/UserItems/Resume?userId={}&Limit={limit}&MediaTypes=Video&Fields={}",
            self.user_id,
            Self::FIELDS
        ))?;
        Ok(r.items)
    }

    /// Every episode of a series, in season and episode order.
    pub fn episodes(&self, series_id: &str) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!(
            "/Shows/{}/Episodes?userId={}&Fields={}",
            enc(series_id),
            self.user_id,
            Self::FIELDS
        ))?;
        Ok(r.items)
    }

    /// Movies, shows and episodes in every library whose name matches `term`,
    /// in the server's relevance order.
    pub fn search(&self, term: &str, limit: usize) -> Result<SearchResults> {
        let r: ItemsResponse = self.get_json(&format!(
            "/Items?userId={}&Recursive=true&SearchTerm={}&IncludeItemTypes=Movie,Series,Episode&Fields={}&Limit={limit}",
            self.user_id,
            enc(term),
            Self::FIELDS
        ))?;
        Ok(group_search_results(r.items))
    }

    /// One item by id.
    pub fn item(&self, id: &str) -> Result<Item> {
        self.get_json(&format!("/Items/{}?userId={}", enc(id), self.user_id))
    }

    /// First playable video in the whole library (used by tests / proof mode).
    pub fn first_video(&self, search: Option<&str>) -> Result<Item> {
        let mut q = format!(
            "/Items?userId={}&Recursive=true&IncludeItemTypes=Movie,Episode&SortBy=SortName&Limit=1&Fields={}",
            self.user_id,
            Self::FIELDS
        );
        if let Some(s) = search {
            q.push_str(&format!("&SearchTerm={}", enc(s)));
        }
        let r: ItemsResponse = self.get_json(&q)?;
        r.items.into_iter().next().ok_or_else(|| anyhow!("no video found"))
    }

    /// URL of an item image (`kind` = `"Primary"`, `"Backdrop"`, …), if it has one.
    pub fn image_url(&self, item: &Item, kind: &str, max_width: u32) -> Option<String> {
        let tag = if kind == "Backdrop" {
            item.backdrop_image_tags.first()?
        } else {
            item.image_tags.get(kind)?
        };
        Some(format!(
            "{}/Items/{}/Images/{kind}?maxWidth={max_width}&quality=90&tag={tag}",
            self.base, item.id
        ))
    }

    /// Download `url` with this session's credentials.
    pub fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &self.auth())
            .call()
            .map_err(|e| anyhow!("GET image: {e}"))?;
        let mut buf = Vec::new();
        let _bytes_read = std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
        Ok(buf)
    }

    /// Direct-play stream URL (no token; auth goes via [`Self::media_auth_header`]).
    /// mpv does all decoding, so no server transcoding.
    pub fn stream_url(&self, item_id: &str) -> String {
        format!("{}/Videos/{}/stream?static=true", self.base, enc(item_id))
    }

    /// Subtitles stored as separate files next to the video. The static stream
    /// only carries embedded tracks, so these must be added to mpv separately.
    pub fn external_subtitles(&self, item_id: &str) -> Result<Vec<crate::player::ExternalSubtitle>> {
        let item: ItemMediaSources = self.get_json(&format!(
            "/Items/{}?userId={}&Fields=MediaSources",
            enc(item_id),
            self.user_id
        ))?;
        // `stream?static=true` plays the first media source.
        let Some(source) = item.media_sources.into_iter().next() else {
            return Ok(Vec::new());
        };
        Ok(self.subtitle_files(item_id, &source, false))
    }

    /// Subtitle tracks of `source` that mpv must load as separate files: the
    /// sidecar files, plus the embedded text tracks when `embedded_too` (a
    /// transcoded stream carries no subtitles, so the server extracts them).
    fn subtitle_files(&self, item_id: &str, source: &MediaSource, embedded_too: bool) -> Vec<crate::player::ExternalSubtitle> {
        source
            .media_streams
            .iter()
            .filter(|s| s.kind == "Subtitle" && s.supports_external_stream && (s.is_external || embedded_too))
            .map(|s| {
                // Keep text formats as-is (ASS styling); anything else Jellyfin converts to SRT.
                let format = match s.codec.as_deref() {
                    Some("ass") => "ass",
                    Some("ssa") => "ssa",
                    Some("webvtt" | "vtt") => "vtt",
                    _ => "srt",
                };
                crate::player::ExternalSubtitle {
                    url: format!(
                        "{}/Videos/{}/{}/Subtitles/{}/0/Stream.{format}",
                        self.base,
                        enc(item_id),
                        enc(&source.id),
                        s.index
                    ),
                    title: s
                        .display_title
                        .clone()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| format!("Subtitle {}", s.index)),
                    language: s.language.clone().unwrap_or_default(),
                }
            })
            .collect()
    }

    /// Send the device's playback settings to the server in one request
    /// (POST /Items/{id}/PlaybackInfo) and return the stream it chose: the file
    /// as-is, or a transcode that fits `settings` and `max_bitrate`.
    pub fn playback_info(
        &self,
        item_id: &str,
        settings: &crate::playback_settings::PlaybackSettings,
        max_bitrate: u64,
        start_secs: f64,
    ) -> Result<PlaybackStream> {
        let response: PlaybackInfoResponse = self.post_for_json(
            &format!("/Items/{}/PlaybackInfo?userId={}", enc(item_id), self.user_id),
            serde_json::json!({
                "UserId": self.user_id,
                "MaxStreamingBitrate": max_bitrate,
                "StartTimeTicks": secs_to_ticks(start_secs),
                "DeviceProfile": settings.device_profile(max_bitrate),
                "EnableDirectPlay": true,
                "EnableDirectStream": true,
                "EnableTranscoding": true,
                "AllowVideoStreamCopy": true,
                "AllowAudioStreamCopy": true,
                "AutoOpenLiveStream": true,
            }),
        )?;
        let source = response
            .media_sources
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("the server offered no media source for this item"))?;
        let (method, url) = if source.supports_direct_play || source.supports_direct_stream {
            let method = if source.supports_direct_play { PlayMethod::DirectPlay } else { PlayMethod::DirectStream };
            let mut url = format!("{}&MediaSourceId={}", self.stream_url(item_id), enc(&source.id));
            if let Some(play_session_id) = &response.play_session_id {
                url.push_str(&format!("&PlaySessionId={}", enc(play_session_id)));
            }
            (method, url)
        } else if let Some(transcoding_url) = &source.transcoding_url {
            (PlayMethod::Transcode, format!("{}{}", self.base, without_credentials(transcoding_url)))
        } else {
            return Err(anyhow!("the server can neither send this video as-is nor convert it for these playback settings"));
        };
        let transcode_reasons = source
            .transcoding_url
            .as_deref()
            .filter(|_| method == PlayMethod::Transcode)
            .and_then(|url| query_value(url, "TranscodeReasons"))
            .map(|reasons| reasons.split(',').map(str::to_string).collect())
            .unwrap_or_default();
        let subtitles = self.subtitle_files(item_id, &source, method == PlayMethod::Transcode);
        Ok(PlaybackStream {
            url,
            identity: StreamIdentity {
                method,
                media_source_id: Some(source.id),
                play_session_id: response.play_session_id,
            },
            transcode_reasons,
            subtitles,
        })
    }

    /// Download speed to the server in bits per second, measured by fetching
    /// test data (GET /Playback/BitrateTest) until a download lasts long enough
    /// to be representative.
    pub fn measure_bitrate(&self) -> Result<u64> {
        let mut measured = 0;
        for size in [500_000u64, 1_000_000, 3_000_000, 10_000_000] {
            let started = std::time::Instant::now();
            let response = self
                .agent
                .get(&format!("{}/Playback/BitrateTest?size={size}", self.base))
                .set("Authorization", &self.auth())
                .call()
                .map_err(|e| anyhow!("speed test: {e}"))?;
            let received = std::io::copy(&mut response.into_reader(), &mut std::io::sink())?;
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            if received == 0 {
                return Err(anyhow!("speed test: the server sent no data"));
            }
            measured = (received as f64 * 8.0 / elapsed) as u64;
            // Short downloads are dominated by latency and underestimate the speed.
            if elapsed >= 1.0 {
                break;
            }
        }
        Ok(measured.min(crate::playback_settings::MAXIMUM_BITRATE))
    }

    /// Stop the server's transcode for `play_session_id` (DELETE /Videos/ActiveEncodings).
    pub fn stop_transcoding(&self, play_session_id: &str) -> Result<()> {
        let path = format!(
            "/Videos/ActiveEncodings?deviceId={}&playSessionId={}",
            enc(&self.device_id),
            enc(play_session_id)
        );
        self.agent
            .delete(&format!("{}{path}", self.base))
            .set("Authorization", &self.auth())
            .call()
            .map(drop)
            .map_err(|e| anyhow!("DELETE {path}: {e}"))
    }

    /// Tell the server playback of `stream` started at `pos` seconds.
    pub fn report_start(&self, item_id: &str, pos: f64, stream: &StreamIdentity) -> Result<()> {
        self.post_json("/Sessions/Playing", report(item_id, pos, false, stream)?)
    }

    /// Report the current position, pause state and stream.
    pub fn report_progress(&self, item_id: &str, pos: f64, paused: bool, stream: &StreamIdentity) -> Result<()> {
        self.post_json("/Sessions/Playing/Progress", report(item_id, pos, paused, stream)?)
    }

    /// Tell the server playback stopped at `pos` seconds, which also ends its transcode.
    pub fn report_stopped(&self, item_id: &str, pos: f64, stream: &StreamIdentity) -> Result<()> {
        self.post_json("/Sessions/Playing/Stopped", report(item_id, pos, false, stream)?)
    }

    /// Raw /Sessions JSON for this device (proof of server-side playback state).
    pub fn sessions_json(&self) -> Result<serde_json::Value> {
        self.get_json(&format!("/Sessions?deviceId={}", enc(&self.device_id)))
    }
}

fn report(item_id: &str, pos: f64, paused: bool, stream: &StreamIdentity) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(PlaybackReport {
        item_id,
        position_ticks: secs_to_ticks(pos),
        is_paused: paused,
        play_method: stream.method.as_str(),
        can_seek: true,
        media_source_id: stream.media_source_id.as_deref(),
        play_session_id: stream.play_session_id.as_deref(),
    })?)
}

fn is_credential(key: &str) -> bool {
    matches!(key.to_ascii_lowercase().as_str(), "api_key" | "apikey" | "token")
}

/// `url` without credential query parameters: mpv sends the token in a header
/// instead, so it never shows up in logs.
fn without_credentials(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !is_credential(pair.split_once('=').map_or(*pair, |(key, _)| key)))
        .collect();
    format!("{path}?{}", kept.join("&"))
}

/// The value of query parameter `key` (case-insensitive), percent-decoded.
fn query_value(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.into_owned())
}

/// Redact any api_key / ApiKey / token query parameter in a URL.
pub fn redact(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let q: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if is_credential(k) => {
                format!("{k}=<redacted>")
            }
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", q.join("&"))
}

/// The episodes before and after `current_id` in `episodes`.
pub fn adjacent_episodes(episodes: &[Item], current_id: &str) -> (Option<Item>, Option<Item>) {
    let Some(index) = episodes.iter().position(|episode| episode.id == current_id) else {
        return (None, None);
    };
    let previous = index.checked_sub(1).and_then(|previous| episodes.get(previous)).cloned();
    let next = episodes.get(index + 1).cloned();
    (previous, next)
}

/// Search hits split by kind, each keeping the order the server returned.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    /// Items of type `"Movie"`.
    pub movies: Vec<Item>,
    /// Items of type `"Series"`.
    pub shows: Vec<Item>,
    /// Items of type `"Episode"`.
    pub episodes: Vec<Item>,
}

impl SearchResults {
    /// Number of results across all groups.
    pub fn len(&self) -> usize {
        self.movies.len() + self.shows.len() + self.episodes.len()
    }

    /// Whether no group has any result.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Group search hits into movies, shows and episodes; other kinds are dropped.
pub fn group_search_results(items: Vec<Item>) -> SearchResults {
    let mut results = SearchResults::default();
    for item in items {
        match item.kind.as_str() {
            "Movie" => results.movies.push(item),
            "Series" => results.shows.push(item),
            "Episode" => results.episodes.push(item),
            _ => {}
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episodes(ids: &[&str]) -> Vec<Item> {
        ids.iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({ "Id": id, "Name": id, "Type": "Episode" }))
                    .expect("valid item json")
            })
            .collect()
    }

    fn item(id: &str, kind: &str) -> Item {
        serde_json::from_value(serde_json::json!({ "Id": id, "Name": id, "Type": kind })).expect("valid item json")
    }

    fn item_ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    fn ids(adjacent: (Option<Item>, Option<Item>)) -> (Option<String>, Option<String>) {
        (adjacent.0.map(|item| item.id), adjacent.1.map(|item| item.id))
    }

    #[test]
    fn finds_adjacent_episodes() {
        let series = episodes(&["e1", "e2", "e3"]);
        assert_eq!(ids(adjacent_episodes(&series, "e1")), (None, Some("e2".into())));
        assert_eq!(ids(adjacent_episodes(&series, "e2")), (Some("e1".into()), Some("e3".into())));
        assert_eq!(ids(adjacent_episodes(&series, "e3")), (Some("e2".into()), None));
        assert_eq!(ids(adjacent_episodes(&series, "missing")), (None, None));
        assert_eq!(ids(adjacent_episodes(&[], "e1")), (None, None));
    }

    #[test]
    fn groups_search_results_by_kind_in_server_order() {
        let results = group_search_results(vec![
            item("m1", "Movie"),
            item("e1", "Episode"),
            item("s1", "Series"),
            item("m2", "Movie"),
            item("b1", "BoxSet"),
            item("e2", "Episode"),
            item("s2", "Series"),
        ]);
        assert_eq!(item_ids(&results.movies), ["m1", "m2"]);
        assert_eq!(item_ids(&results.shows), ["s1", "s2"]);
        assert_eq!(item_ids(&results.episodes), ["e1", "e2"]);
        assert_eq!(results.len(), 6);
        assert!(!results.is_empty());
    }

    #[test]
    fn strips_credentials_from_transcoding_urls() {
        let url = "/videos/abc/master.m3u8?&DeviceId=d&PlaySessionId=p&ApiKey=secret&api_key=x&TranscodeReasons=VideoCodecNotSupported%2CAudioCodecNotSupported";
        let stripped = without_credentials(url);
        assert_eq!(
            stripped,
            "/videos/abc/master.m3u8?DeviceId=d&PlaySessionId=p&TranscodeReasons=VideoCodecNotSupported%2CAudioCodecNotSupported"
        );
        assert_eq!(
            query_value(&stripped, "transcodereasons").as_deref(),
            Some("VideoCodecNotSupported,AudioCodecNotSupported")
        );
        assert_eq!(query_value(&stripped, "Missing"), None);
        assert_eq!(without_credentials("/plain"), "/plain");
    }

    #[test]
    fn reports_name_the_stream() -> anyhow::Result<()> {
        let stream = StreamIdentity {
            method: PlayMethod::Transcode,
            media_source_id: Some("source".into()),
            play_session_id: Some("session".into()),
        };
        let body = report("item", 1.5, true, &stream)?;
        assert_eq!(body["PlayMethod"], "Transcode");
        assert_eq!(body["PlaySessionId"], "session");
        assert_eq!(body["MediaSourceId"], "source");
        assert_eq!(body["PositionTicks"], 15_000_000);
        let direct = report("item", 0.0, false, &StreamIdentity::default())?;
        assert_eq!(direct["PlayMethod"], "DirectPlay");
        assert!(direct.get("PlaySessionId").is_none());
        Ok(())
    }

    #[test]
    fn groups_no_search_results() {
        let results = group_search_results(vec![]);
        assert!(results.is_empty());
        assert_eq!(results.len(), 0);
        assert!(group_search_results(vec![item("f1", "Folder")]).is_empty());
    }
}
