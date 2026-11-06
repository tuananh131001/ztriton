//! Playback preferences remembered across sessions, stored as JSON in
//! [`crate::jellyfin::config_dir`].

use crate::player::SubtitlePreference;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default)]
    pub subtitle: Option<SubtitlePreference>,
}

fn path() -> PathBuf {
    crate::jellyfin::config_dir().join("preferences.json")
}

impl Preferences {
    /// A missing file means nothing has been chosen yet.
    pub fn load() -> Result<Self> {
        let path = path();
        match std::fs::read_to_string(&path) {
            Ok(json) => serde_json::from_str(&json).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-write never leaves a truncated file.
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json).with_context(|| format!("write {}", temporary.display()))?;
        std::fs::rename(&temporary, &path).with_context(|| format!("write {}", path.display()))
    }

    pub fn save_subtitle(subtitle: SubtitlePreference) -> Result<()> {
        let mut preferences = Self::load().unwrap_or_else(|e| {
            log::warn!("ignoring unreadable preferences: {e:#}");
            Self::default()
        });
        preferences.subtitle = Some(subtitle);
        preferences.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_subtitle_choice() -> Result<()> {
        let preferences = Preferences {
            subtitle: Some(SubtitlePreference::Track { language: "vie".into(), title: "Vietnamese".into() }),
        };
        let json = serde_json::to_string(&preferences)?;
        assert_eq!(serde_json::from_str::<Preferences>(&json)?, preferences);
        let off: Preferences = serde_json::from_str(r#"{"subtitle":{"kind":"off"}}"#)?;
        assert_eq!(off.subtitle, Some(SubtitlePreference::Off));
        assert_eq!(serde_json::from_str::<Preferences>("{}")?, Preferences::default());
        Ok(())
    }
}
