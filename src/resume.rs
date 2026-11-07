//! Where to resume each video, remembered on this Mac as JSON in
//! [`crate::jellyfin::config_dir`].
//!
//! The Jellyfin server alone cannot be relied on: it discards any position
//! under its `MinResumePct` (5% of the runtime by default, 6½ minutes of a
//! two-hour movie), and on Cmd-Q the app gets only ~100 ms to report the stop.

use crate::jellyfin::Item;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Past this fraction of the runtime a video counts as watched, so it starts
/// over next time (Jellyfin's default `MaxResumePct` is 90).
const FINISHED_FRACTION: f64 = 0.9;

/// Jellyfin bumps `LastPlayedDate` when playback starts (some versions also
/// on progress and stop reports), and the app reports right after saving here,
/// so a server date only this much newer may still be this Mac's own report.
/// Also absorbs clock skew with the server.
const OWN_REPORT_GRACE_SECS: f64 = 60.0;

/// Our own report leaves the server at our position (or 0, under its minimum
/// resume percentage); progress is reported every 10 s.
const OWN_REPORT_POSITION_TOLERANCE_SECS: f64 = 15.0;

/// The last position this Mac played a video to.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedPosition {
    /// Where to resume, in seconds; 0 once the video was watched to the end.
    pub position_secs: f64,
    /// When the position was saved, in seconds since the Unix epoch.
    pub saved_at: f64,
}

/// Saved positions keyed by user and item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResumeStore {
    #[serde(default)]
    positions: HashMap<String, SavedPosition>,
}

fn path() -> PathBuf {
    crate::jellyfin::config_dir().join("resume.json")
}

fn key(user_id: &str, item_id: &str) -> String {
    format!("{user_id}/{item_id}")
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

impl ResumeStore {
    /// A missing file means nothing has been played yet.
    pub fn load() -> Result<Self> {
        Self::load_from(&path())
    }

    fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(json) => serde_json::from_str(&json).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Write-then-rename so quitting mid-write never leaves a truncated file.
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json).with_context(|| format!("write {}", temporary.display()))?;
        std::fs::rename(&temporary, path).with_context(|| format!("write {}", path.display()))
    }

    /// The saved position of `item_id` for `user_id`, if this Mac played it.
    pub fn get(&self, user_id: &str, item_id: &str) -> Option<SavedPosition> {
        self.positions.get(&key(user_id, item_id)).copied()
    }

    fn record(&mut self, user_id: &str, item_id: &str, position_secs: f64, duration_secs: f64, saved_at: f64) {
        let finished = duration_secs > 0.0 && position_secs >= duration_secs * FINISHED_FRACTION;
        let position_secs = if finished { 0.0 } else { position_secs.max(0.0) };
        let _previous = self.positions.insert(key(user_id, item_id), SavedPosition { position_secs, saved_at });
    }

    /// Where `item` should start: the newer of this Mac's saved position and
    /// the server's, so playing further on another device still wins.
    pub fn start_position(&self, user_id: &str, item: &Item) -> f64 {
        choose_start(item, self.get(user_id, &item.id))
    }
}

/// Save that `item_id` was played to `position_secs` of `duration_secs`.
pub fn remember(user_id: &str, item_id: &str, position_secs: f64, duration_secs: f64) -> Result<()> {
    let path = path();
    let mut store = ResumeStore::load_from(&path).unwrap_or_else(|e| {
        log::warn!("ignoring unreadable resume positions: {e:#}");
        ResumeStore::default()
    });
    store.record(user_id, item_id, position_secs, duration_secs, now());
    store.save_to(&path)
}

fn choose_start(item: &Item, saved: Option<SavedPosition>) -> f64 {
    let server = item.resume_secs();
    let Some(saved) = saved else { return server };
    let server_played_at = item
        .user_data
        .as_ref()
        .and_then(|data| data.last_played_date.as_deref())
        .and_then(parse_utc_timestamp);
    let Some(played_at) = server_played_at else { return saved.position_secs };
    if played_at <= saved.saved_at {
        return saved.position_secs;
    }
    let could_be_own_report = played_at <= saved.saved_at + OWN_REPORT_GRACE_SECS
        && (server == 0.0 || (server - saved.position_secs).abs() <= OWN_REPORT_POSITION_TOLERANCE_SECS);
    if could_be_own_report { saved.position_secs } else { server }
}

/// Seconds since the Unix epoch of a UTC timestamp as Jellyfin writes it,
/// e.g. `2026-11-07T02:36:01.430715Z`.
fn parse_utc_timestamp(text: &str) -> Option<f64> {
    let text = text.strip_suffix('Z').or_else(|| text.strip_suffix("+00:00"))?;
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date_parts.next()??, date_parts.next()??, date_parts.next()??);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut clock_parts = clock.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock_parts.next()??, clock_parts.next()??, clock_parts.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let fraction = if fraction.is_empty() {
        0.0
    } else if fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("0.{fraction}").parse::<f64>().ok()?
    } else {
        return None;
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds as f64 + fraction)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jellyfin::UserData;

    const MOONFALL_SECS: f64 = 7817.8525;

    fn movie(server_position_secs: f64, last_played_date: Option<&str>) -> Item {
        let mut item: Item = serde_json::from_value(serde_json::json!({
            "Id": "moonfall",
            "Name": "Moonfall",
            "Type": "Movie",
            "RunTimeTicks": crate::jellyfin::secs_to_ticks(MOONFALL_SECS),
        }))
        .expect("valid item json");
        item.user_data = Some(UserData {
            playback_position_ticks: crate::jellyfin::secs_to_ticks(server_position_secs),
            last_played_date: last_played_date.map(str::to_string),
            ..Default::default()
        });
        item
    }

    #[test]
    fn parses_jellyfin_timestamps() {
        assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_utc_timestamp("2000-02-29T23:59:59Z"), Some(951_868_799.0));
        let parsed = parse_utc_timestamp("2026-11-07T02:36:01.430715Z").expect("valid timestamp");
        assert!((parsed - 1_794_018_961.430_715).abs() < 1e-3, "{parsed}");
        assert_eq!(parse_utc_timestamp("2026-11-07T02:36:01+00:00"), Some(1_794_018_961.0));
        for invalid in ["", "2026-11-07", "2026-11-07T02:36:01", "2026-13-07T02:36:01Z", "2026-11-07T02:36:01.x1Z"] {
            assert_eq!(parse_utc_timestamp(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn short_watch_resumes_although_the_server_forgot_it() {
        // Jellyfin stores 0 for a 2-minute stop of Moonfall (under 5% of the runtime).
        let item = movie(0.0, Some("2026-11-07T02:34:02.467881Z"));
        let saved = SavedPosition { position_secs: 120.0, saved_at: 1_794_018_954.0 };
        assert_eq!(choose_start(&item, Some(saved)), 120.0);
    }

    #[test]
    fn own_stop_report_does_not_override_the_saved_position() {
        // Saved here at 02:46:38.167; the stop report then reset the server to 0
        // (under 5%) and bumped LastPlayedDate to 02:46:38.205.
        let item = movie(0.0, Some("2026-11-07T02:46:38.2050484Z"));
        let saved = SavedPosition { position_secs: 120.0, saved_at: 1_794_019_598.167 };
        assert_eq!(choose_start(&item, Some(saved)), 120.0);
    }

    #[test]
    fn quick_handoff_to_another_device_is_not_mistaken_for_our_report() {
        // Saved here at 02:46:38; another device started 30 s later and is at 50 min.
        let item = movie(3000.0, Some("2026-11-07T02:47:08Z"));
        let saved = SavedPosition { position_secs: 120.0, saved_at: 1_794_019_598.0 };
        assert_eq!(choose_start(&item, Some(saved)), 3000.0);
    }

    #[test]
    fn own_progress_report_echo_keeps_the_saved_position() {
        let item = movie(600.0, Some("2026-11-07T02:46:39Z"));
        let saved = SavedPosition { position_secs: 605.0, saved_at: 1_794_019_598.0 };
        assert_eq!(choose_start(&item, Some(saved)), 605.0);
    }

    #[test]
    fn server_position_wins_when_played_elsewhere_later() {
        let item = movie(3000.0, Some("2026-11-07T10:00:00Z"));
        let saved = SavedPosition { position_secs: 120.0, saved_at: 1_794_018_954.0 };
        assert_eq!(choose_start(&item, Some(saved)), 3000.0);
    }

    #[test]
    fn server_position_is_used_without_a_saved_one() {
        assert_eq!(choose_start(&movie(600.0, None), None), 600.0);
        assert_eq!(choose_start(&movie(0.0, None), None), 0.0);
    }

    #[test]
    fn rewinding_here_beats_an_older_server_position() {
        let item = movie(3000.0, Some("2026-11-07T02:34:02Z"));
        let saved = SavedPosition { position_secs: 90.0, saved_at: 1_794_018_954.0 };
        assert_eq!(choose_start(&item, Some(saved)), 90.0);
    }

    #[test]
    fn finishing_starts_over_even_if_the_server_missed_the_stop() {
        let mut store = ResumeStore::default();
        store.record("user", "moonfall", MOONFALL_SECS - 60.0, MOONFALL_SECS, 2_000_000_000.0);
        let saved = store.get("user", "moonfall").expect("recorded");
        assert_eq!(saved.position_secs, 0.0);
        // The server still holds the last progress report from before the end.
        assert_eq!(choose_start(&movie(7000.0, Some("2026-11-07T02:34:02Z")), Some(saved)), 0.0);
    }

    #[test]
    fn positions_are_per_user_and_survive_a_reload() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("ztriton-resume-test-{}", std::process::id()));
        let path = dir.join("resume.json");
        let mut store = ResumeStore::load_from(&path)?;
        assert_eq!(store, ResumeStore::default());
        store.record("alice", "moonfall", 120.5, MOONFALL_SECS, 1.0);
        store.save_to(&path)?;
        let reloaded = ResumeStore::load_from(&path)?;
        std::fs::remove_dir_all(&dir)?;
        assert_eq!(reloaded.get("alice", "moonfall"), Some(SavedPosition { position_secs: 120.5, saved_at: 1.0 }));
        assert_eq!(reloaded.get("bob", "moonfall"), None);
        assert_eq!(reloaded.start_position("alice", &movie(0.0, None)), 120.5);
        Ok(())
    }
}
