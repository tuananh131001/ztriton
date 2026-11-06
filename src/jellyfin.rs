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
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &id);
    id
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Clone)]
pub struct Jellyfin {
    pub base: String,
    token: String,
    pub device_id: String,
    pub user_id: String,
    agent: ureq::Agent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub collection_type: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub production_year: Option<i32>,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub community_rating: Option<f32>,
    #[serde(default)]
    pub official_rating: Option<String>,
    #[serde(default)]
    pub series_name: Option<String>,
    #[serde(default)]
    pub parent_index_number: Option<i32>,
    #[serde(default)]
    pub index_number: Option<i32>,
    #[serde(default)]
    pub is_folder: bool,
    #[serde(default)]
    pub image_tags: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub backdrop_image_tags: Vec<String>,
    #[serde(default)]
    pub user_data: Option<UserData>,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct UserData {
    #[serde(default)]
    pub playback_position_ticks: i64,
    #[serde(default)]
    pub played_percentage: Option<f64>,
    #[serde(default)]
    pub played: bool,
}

impl Item {
    pub fn is_playable(&self) -> bool {
        matches!(self.kind.as_str(), "Movie" | "Episode" | "Video" | "MusicVideo")
    }

    pub fn runtime_secs(&self) -> Option<f64> {
        self.run_time_ticks.map(ticks_to_secs)
    }

    pub fn resume_secs(&self) -> f64 {
        self.user_data
            .as_ref()
            .map(|u| ticks_to_secs(u.playback_position_ticks))
            .unwrap_or(0.0)
    }

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
}

pub fn secs_to_ticks(s: f64) -> i64 {
    (s * 10_000_000.0) as i64
}

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
            .map_err(|_| anyhow!("{base} did not answer like a Jellyfin server"))?;
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

    fn post_json(&self, path: &str, body: serde_json::Value) -> Result<()> {
        self.agent
            .post(&format!("{}{}", self.base, path))
            .set("Authorization", &self.auth())
            .send_json(body)
            .map_err(|e| anyhow!("POST {path}: {e}"))?;
        Ok(())
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

    pub fn views(&self) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!("/UserViews?userId={}", self.user_id))?;
        Ok(r.items)
    }

    pub fn children(&self, parent_id: &str) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!(
            "/Items?userId={}&ParentId={}&SortBy=SortName&SortOrder=Ascending&Fields={}&Limit=500",
            self.user_id,
            enc(parent_id),
            Self::FIELDS
        ))?;
        Ok(r.items)
    }

    pub fn latest(&self, parent_id: &str, limit: usize) -> Result<Vec<Item>> {
        // /Items/Latest returns a bare array.
        self.get_json(&format!(
            "/Items/Latest?userId={}&ParentId={}&Limit={limit}&Fields={}",
            self.user_id,
            enc(parent_id),
            Self::FIELDS
        ))
    }

    pub fn resume(&self, limit: usize) -> Result<Vec<Item>> {
        let r: ItemsResponse = self.get_json(&format!(
            "/UserItems/Resume?userId={}&Limit={limit}&MediaTypes=Video&Fields={}",
            self.user_id,
            Self::FIELDS
        ))?;
        Ok(r.items)
    }

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

    pub fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &self.auth())
            .call()
            .map_err(|e| anyhow!("GET image: {e}"))?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
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
        Ok(source
            .media_streams
            .into_iter()
            .filter(|s| s.kind == "Subtitle" && s.is_external && s.supports_external_stream)
            .map(|s| {
                // Keep text formats as-is (ASS styling); anything else Jellyfin converts to SRT.
                let format = match s.codec.as_deref() {
                    Some("ass") => "ass",
                    Some("ssa") => "ssa",
                    Some("webvtt" | "vtt") => "vtt",
                    _ => "srt",
                };
                let language = s.language.unwrap_or_default();
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
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| format!("Subtitle {}", s.index)),
                    language,
                }
            })
            .collect())
    }

    pub fn report_start(&self, item_id: &str, pos: f64) -> Result<()> {
        self.post_json("/Sessions/Playing", self.report(item_id, pos, false))
    }

    pub fn report_progress(&self, item_id: &str, pos: f64, paused: bool) -> Result<()> {
        self.post_json("/Sessions/Playing/Progress", self.report(item_id, pos, paused))
    }

    pub fn report_stopped(&self, item_id: &str, pos: f64) -> Result<()> {
        self.post_json("/Sessions/Playing/Stopped", self.report(item_id, pos, false))
    }

    fn report(&self, item_id: &str, pos: f64, paused: bool) -> serde_json::Value {
        serde_json::to_value(PlaybackReport {
            item_id,
            position_ticks: secs_to_ticks(pos),
            is_paused: paused,
            play_method: "DirectPlay",
            can_seek: true,
        })
        .expect("serializable")
    }

    /// Raw /Sessions JSON for this device (proof of server-side playback state).
    pub fn sessions_json(&self) -> Result<serde_json::Value> {
        self.get_json(&format!("/Sessions?deviceId={}", enc(&self.device_id)))
    }
}

/// Redact any api_key / ApiKey / token query parameter in a URL.
pub fn redact(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let q: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if matches!(k.to_ascii_lowercase().as_str(), "api_key" | "apikey" | "token") => {
                format!("{k}=<redacted>")
            }
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", q.join("&"))
}
