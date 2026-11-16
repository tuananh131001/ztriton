//! What this device tells Jellyfin it can play. The server compares that with
//! the file and either sends it as-is (direct play) or converts it on the fly
//! (transcode).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// "Maximum" bitrate: 360 Mbps, high enough to never limit a real file.
pub const MAXIMUM_BITRATE: u64 = 360_000_000;

/// The largest picture the device accepts; bigger videos are downscaled by the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxResolution {
    /// No limit.
    #[default]
    Max,
    /// 3840×2160.
    Uhd4k,
    /// 2560×1440.
    Qhd1440,
    /// 1920×1080.
    Fhd1080,
    /// 1280×720.
    Hd720,
    /// 854×480.
    Sd480,
    /// 640×360.
    Sd360,
}

impl MaxResolution {
    /// Every choice, largest first.
    pub const ALL: [Self; 7] = [Self::Max, Self::Uhd4k, Self::Qhd1440, Self::Fhd1080, Self::Hd720, Self::Sd480, Self::Sd360];

    /// Name shown in menus.
    pub fn label(self) -> &'static str {
        match self {
            Self::Max => "Max",
            Self::Uhd4k => "4K",
            Self::Qhd1440 => "1440p",
            Self::Fhd1080 => "1080p",
            Self::Hd720 => "720p",
            Self::Sd480 => "480p",
            Self::Sd360 => "360p",
        }
    }

    /// Largest accepted `(width, height)`, or `None` for no limit.
    pub fn max_size(self) -> Option<(u32, u32)> {
        match self {
            Self::Max => None,
            Self::Uhd4k => Some((3840, 2160)),
            Self::Qhd1440 => Some((2560, 1440)),
            Self::Fhd1080 => Some((1920, 1080)),
            Self::Hd720 => Some((1280, 720)),
            Self::Sd480 => Some((854, 480)),
            Self::Sd360 => Some((640, 360)),
        }
    }
}

/// How much data per second the device accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bitrate {
    /// Measure the download speed before each play and use that.
    #[default]
    Auto,
    /// A fixed limit in bits per second; [`MAXIMUM_BITRATE`] is "Maximum".
    Limit(u64),
}

impl Bitrate {
    /// Every choice, from Auto and Maximum down to the lowest limit.
    pub const ALL: [Self; 14] = [
        Self::Auto,
        Self::Limit(MAXIMUM_BITRATE),
        Self::Limit(120_000_000),
        Self::Limit(80_000_000),
        Self::Limit(60_000_000),
        Self::Limit(40_000_000),
        Self::Limit(20_000_000),
        Self::Limit(15_000_000),
        Self::Limit(10_000_000),
        Self::Limit(8_000_000),
        Self::Limit(4_000_000),
        Self::Limit(2_000_000),
        Self::Limit(1_000_000),
        Self::Limit(500_000),
    ];

    /// Name shown in menus.
    pub fn label(self) -> String {
        match self {
            Self::Auto => "Auto".into(),
            Self::Limit(MAXIMUM_BITRATE) => "Maximum".into(),
            Self::Limit(bits_per_second) => format_bitrate(bits_per_second),
        }
    }
}

/// `"20 Mbps"`, `"1.5 Mbps"` or `"500 kbps"`.
pub fn format_bitrate(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000 {
        let megabits = bits_per_second as f64 / 1_000_000.0;
        if megabits >= 10.0 || megabits.fract() == 0.0 {
            format!("{megabits:.0} Mbps")
        } else {
            format!("{megabits:.1} Mbps")
        }
    } else {
        format!("{} kbps", bits_per_second / 1000)
    }
}

/// Which formats the device claims it can play.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    /// Whatever mpv plays natively.
    #[default]
    Auto,
    /// Only H.264 (8-bit) with AAC audio; everything else is converted.
    Compatible,
    /// Claims to play anything, so the server never converts the format.
    DirectPlay,
    /// The user's own container and codec lists.
    Custom,
}

impl Compatibility {
    /// Every choice.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Compatible, Self::DirectPlay, Self::Custom];

    /// Name shown in menus.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Compatible => "Compatible",
            Self::DirectPlay => "Direct Play",
            Self::Custom => "Custom",
        }
    }

    /// One-line explanation shown under the choices.
    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "Everything the player supports natively plays as-is.",
            Self::Compatible => "Only H.264 video with AAC audio plays as-is; everything else is converted.",
            Self::DirectPlay => "Claims to play anything, so the server never converts the format.",
            Self::Custom => "Only the containers and codecs listed below play as-is.",
        }
    }
}

/// Format rules for [`Compatibility::Custom`]: comma-separated Jellyfin names
/// (e.g. `"mkv,mp4"`, `"h264,hevc"`, `"aac,ac3"`). An empty list means any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomFormats {
    /// Containers played as-is.
    pub containers: String,
    /// Video codecs played as-is.
    pub video_codecs: String,
    /// Audio codecs played as-is.
    pub audio_codecs: String,
}

impl Default for CustomFormats {
    fn default() -> Self {
        Self {
            containers: "mp4,m4v,mov,mkv".into(),
            video_codecs: "h264,hevc".into(),
            audio_codecs: "aac,ac3,eac3,mp3".into(),
        }
    }
}

/// The four playback settings, saved in [`crate::preferences::Preferences`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackSettings {
    /// Largest picture size accepted.
    #[serde(default)]
    pub resolution: MaxResolution,
    /// Most data per second accepted.
    #[serde(default)]
    pub bitrate: Bitrate,
    /// Which formats are claimed.
    #[serde(default)]
    pub compatibility: Compatibility,
    /// Rules used when `compatibility` is [`Compatibility::Custom`].
    #[serde(default)]
    pub custom: CustomFormats,
    /// Convert HDR and Dolby Vision video to SDR.
    #[serde(default)]
    pub force_hdr_transcode: bool,
}

// Formats libmpv (FFmpeg) decodes on macOS, in Jellyfin's naming.
const NATIVE_CONTAINERS: &str = "mkv,webm,mp4,m4v,mov,avi,ts,m2ts,mpegts,flv,wmv,asf,ogg,ogv,3gp,mpg,mpeg,vob";
const NATIVE_VIDEO_CODECS: &str = "h264,hevc,av1,vp8,vp9,mpeg1video,mpeg2video,mpeg4,msmpeg4v3,vc1,wmv3,theora";
const NATIVE_AUDIO_CODECS: &str =
    "aac,mp3,mp2,ac3,eac3,dts,truehd,flac,alac,opus,vorbis,wmav2,wmapro,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be";

const TEXT_SUBTITLES: [&str; 6] = ["srt", "subrip", "ass", "ssa", "vtt", "webvtt"];
const EMBEDDED_SUBTITLES: [&str; 10] =
    ["srt", "subrip", "ass", "ssa", "vtt", "webvtt", "mov_text", "pgssub", "dvdsub", "dvbsub"];

fn condition(condition: &str, property: &str, value: impl ToString) -> Value {
    // Not required: a stream whose value the server doesn't know still plays as-is.
    json!({ "Condition": condition, "Property": property, "Value": value.to_string(), "IsRequired": false })
}

impl PlaybackSettings {
    /// The Jellyfin `DeviceProfile` describing what this device accepts, with
    /// streams limited to `max_bitrate` bits per second.
    pub fn device_profile(&self, max_bitrate: u64) -> Value {
        let direct_play = match self.compatibility {
            Compatibility::Auto => json!({
                "Type": "Video",
                "Container": NATIVE_CONTAINERS,
                "VideoCodec": NATIVE_VIDEO_CODECS,
                "AudioCodec": NATIVE_AUDIO_CODECS,
            }),
            Compatibility::Compatible => json!({
                "Type": "Video",
                "Container": "mp4,m4v,mov,mkv",
                "VideoCodec": "h264",
                "AudioCodec": "aac",
            }),
            // No container or codec list matches every file.
            Compatibility::DirectPlay => json!({ "Type": "Video" }),
            Compatibility::Custom => json!({
                "Type": "Video",
                "Container": normalize_list(&self.custom.containers),
                "VideoCodec": normalize_list(&self.custom.video_codecs),
                "AudioCodec": normalize_list(&self.custom.audio_codecs),
            }),
        };

        let mut codec_profiles = Vec::new();
        let mut video_conditions = Vec::new();
        if let Some((width, height)) = self.resolution.max_size() {
            video_conditions.push(condition("LessThanEqual", "Width", width));
            video_conditions.push(condition("LessThanEqual", "Height", height));
        }
        if self.force_hdr_transcode {
            video_conditions.push(condition("EqualsAny", "VideoRangeType", "SDR"));
        }
        if !video_conditions.is_empty() {
            // No codec: the limits apply to every video, including the transcoded one.
            codec_profiles.push(json!({ "Type": "Video", "Conditions": video_conditions }));
        }
        if self.compatibility == Compatibility::Compatible {
            codec_profiles.push(json!({
                "Type": "Video",
                "Codec": "h264",
                "Conditions": [
                    condition("EqualsAny", "VideoProfile", "high|main|baseline|constrained baseline"),
                    condition("LessThanEqual", "VideoBitDepth", 8),
                ],
            }));
        }

        let max_audio_channels = if self.compatibility == Compatibility::Compatible { "2" } else { "6" };
        let mut subtitle_profiles: Vec<Value> = TEXT_SUBTITLES
            .iter()
            .map(|format| json!({ "Format": format, "Method": "External" }))
            .collect();
        subtitle_profiles.extend(EMBEDDED_SUBTITLES.iter().map(|format| json!({ "Format": format, "Method": "Embed" })));

        json!({
            "Name": "ztriton",
            "MaxStreamingBitrate": max_bitrate,
            "MaxStaticBitrate": max_bitrate,
            "MusicStreamingTranscodingBitrate": 384_000,
            "DirectPlayProfiles": [direct_play],
            "TranscodingProfiles": [{
                "Type": "Video",
                "Container": "ts",
                "VideoCodec": "h264",
                "AudioCodec": "aac",
                "Protocol": "hls",
                "Context": "Streaming",
                "MaxAudioChannels": max_audio_channels,
                "MinSegments": 1,
                "BreakOnNonKeyFrames": true,
            }],
            "ContainerProfiles": [],
            "CodecProfiles": codec_profiles,
            "SubtitleProfiles": subtitle_profiles,
        })
    }
}

/// Lowercase, trimmed, comma-separated, with empty entries dropped.
fn normalize_list(list: &str) -> String {
    list.split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conditions(profile: &Value) -> Vec<(String, String, String)> {
        profile["CodecProfiles"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|codec_profile| codec_profile["Conditions"].as_array().cloned().unwrap_or_default())
            .map(|c| {
                let field = |name: &str| c[name].as_str().unwrap_or_default().to_string();
                (field("Condition"), field("Property"), field("Value"))
            })
            .collect()
    }

    #[test]
    fn default_settings_claim_native_formats_without_limits() {
        let profile = PlaybackSettings::default().device_profile(MAXIMUM_BITRATE);
        assert_eq!(profile["MaxStreamingBitrate"], MAXIMUM_BITRATE);
        assert_eq!(profile["DirectPlayProfiles"][0]["Container"], NATIVE_CONTAINERS);
        assert_eq!(profile["DirectPlayProfiles"][0]["VideoCodec"], NATIVE_VIDEO_CODECS);
        assert!(conditions(&profile).is_empty());
        assert_eq!(profile["TranscodingProfiles"][0]["Protocol"], "hls");
        assert_eq!(profile["TranscodingProfiles"][0]["VideoCodec"], "h264");
    }

    #[test]
    fn resolution_and_hdr_become_video_conditions() {
        let settings = PlaybackSettings {
            resolution: MaxResolution::Hd720,
            force_hdr_transcode: true,
            ..Default::default()
        };
        let profile = settings.device_profile(4_000_000);
        assert_eq!(profile["MaxStreamingBitrate"], 4_000_000);
        assert_eq!(
            conditions(&profile),
            [
                ("LessThanEqual".into(), "Width".into(), "1280".into()),
                ("LessThanEqual".into(), "Height".into(), "720".into()),
                ("EqualsAny".into(), "VideoRangeType".into(), "SDR".into()),
            ]
        );
    }

    #[test]
    fn compatibility_modes_change_direct_play_formats() {
        let profile = |compatibility, custom: CustomFormats| {
            PlaybackSettings { compatibility, custom, ..Default::default() }.device_profile(MAXIMUM_BITRATE)
        };
        let compatible = profile(Compatibility::Compatible, CustomFormats::default());
        assert_eq!(compatible["DirectPlayProfiles"][0]["VideoCodec"], "h264");
        assert_eq!(compatible["DirectPlayProfiles"][0]["AudioCodec"], "aac");
        assert_eq!(compatible["TranscodingProfiles"][0]["MaxAudioChannels"], "2");
        assert!(conditions(&compatible).contains(&("LessThanEqual".into(), "VideoBitDepth".into(), "8".into())));

        let direct = profile(Compatibility::DirectPlay, CustomFormats::default());
        assert_eq!(direct["DirectPlayProfiles"], json!([{ "Type": "Video" }]));

        let custom = profile(
            Compatibility::Custom,
            CustomFormats { containers: " MKV, ,webm ".into(), video_codecs: "av1".into(), audio_codecs: "".into() },
        );
        assert_eq!(custom["DirectPlayProfiles"][0]["Container"], "mkv,webm");
        assert_eq!(custom["DirectPlayProfiles"][0]["VideoCodec"], "av1");
        assert_eq!(custom["DirectPlayProfiles"][0]["AudioCodec"], "");
    }

    #[test]
    fn labels() {
        assert_eq!(Bitrate::Auto.label(), "Auto");
        assert_eq!(Bitrate::Limit(MAXIMUM_BITRATE).label(), "Maximum");
        assert_eq!(Bitrate::Limit(20_000_000).label(), "20 Mbps");
        assert_eq!(format_bitrate(1_500_000), "1.5 Mbps");
        assert_eq!(Bitrate::Limit(500_000).label(), "500 kbps");
        assert_eq!(MaxResolution::Uhd4k.label(), "4K");
    }

    #[test]
    fn serializes_settings() -> anyhow::Result<()> {
        let settings = PlaybackSettings {
            resolution: MaxResolution::Fhd1080,
            bitrate: Bitrate::Limit(8_000_000),
            compatibility: Compatibility::Custom,
            custom: CustomFormats { containers: "mkv".into(), video_codecs: "hevc".into(), audio_codecs: "eac3".into() },
            force_hdr_transcode: true,
        };
        let json = serde_json::to_string(&settings)?;
        assert_eq!(serde_json::from_str::<PlaybackSettings>(&json)?, settings);
        assert_eq!(serde_json::from_str::<PlaybackSettings>("{}")?, PlaybackSettings::default());
        Ok(())
    }
}
