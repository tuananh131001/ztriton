//! Settings screen: the playback settings sent to the server when a video plays.

use super::text_input::{self, TextInput};
use super::*;
use gpui::{AnyElement, Context, ElementId, Entity, FontWeight, IntoElement, Render, SharedString, Window, div, prelude::*, px};
use ztriton::playback_settings::{Bitrate, Compatibility, CustomFormats, MaxResolution, PlaybackSettings};
use ztriton::preferences::Preferences;

pub struct SettingsView {
    settings: PlaybackSettings,
    containers: Entity<TextInput>,
    video_codecs: Entity<TextInput>,
    audio_codecs: Entity<TextInput>,
    error: Option<String>,
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // A few hundred bytes on local disk, read like the resume positions are.
        let (settings, error) = match Preferences::load() {
            Ok(preferences) => (preferences.playback, None),
            Err(e) => (PlaybackSettings::default(), Some(format!("Couldn't read saved settings, showing defaults: {e:#}"))),
        };
        let field = |placeholder: &'static str, text: &str, cx: &mut Context<Self>| {
            let input = cx.new(|cx| {
                let mut input = TextInput::new(placeholder, false, cx);
                input.set_text(text.to_string(), cx);
                input
            });
            cx.subscribe(&input, |this, _, _: &text_input::Changed, cx| this.custom_formats_changed(cx)).detach();
            input
        };
        let containers = field("mkv,mp4", &settings.custom.containers, cx);
        let video_codecs = field("h264,hevc", &settings.custom.video_codecs, cx);
        let audio_codecs = field("aac,ac3", &settings.custom.audio_codecs, cx);
        Self { settings, containers, video_codecs, audio_codecs, error }
    }

    fn custom_formats_changed(&mut self, cx: &mut Context<Self>) {
        let custom = CustomFormats {
            containers: self.containers.read(cx).text(),
            video_codecs: self.video_codecs.read(cx).text(),
            audio_codecs: self.audio_codecs.read(cx).text(),
        };
        if custom != self.settings.custom {
            self.update(cx, |settings| settings.custom = custom);
        }
    }

    fn update(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut PlaybackSettings)) {
        change(&mut self.settings);
        // Saved on the UI thread so saves land in order; the file is tiny.
        match Preferences::save_playback(self.settings.clone()) {
            Ok(()) => {
                proof_log(format!("settings: saved {:?}", self.settings));
                self.error = None;
            }
            Err(e) => {
                log::warn!("saving playback settings: {e:#}");
                self.error = Some(format!("Couldn't save settings: {e:#}"));
            }
        }
        cx.notify();
    }

    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[T],
        selected: T,
        label: impl Fn(T) -> String,
        apply: fn(&mut PlaybackSettings, T),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div().flex().flex_wrap().gap_2().children(options.iter().copied().enumerate().map(|(index, option)| {
            let chosen = option == selected;
            div()
                .id(ElementId::from((id, index)))
                .px_3()
                .py_1()
                .rounded_full()
                .text_sm()
                .cursor_pointer()
                .bg(if chosen { accent() } else { surface_hi() })
                .text_color(if chosen { bg() } else { text() })
                .when(!chosen, |d| d.hover(|s| s.bg(surface())))
                .child(label(option))
                .on_click(cx.listener(move |this, _, _, cx| this.update(cx, |settings| apply(settings, option))))
        }))
        .into_any_element()
    }

    fn section(title: &'static str, description: impl Into<SharedString>, body: impl IntoElement) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().text_sm().text_color(text_dim()).child(description.into()))
            .child(body)
            .into_any_element()
    }

    fn custom_field(label: &'static str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(div().w(px(110.)).text_sm().text_color(text_dim()).child(label))
            .child(div().w(px(360.)).child(input.clone()))
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.clone();
        let resolution = self.choices(
            "resolution",
            &MaxResolution::ALL,
            settings.resolution,
            |resolution| resolution.label().to_string(),
            |settings, resolution| settings.resolution = resolution,
            cx,
        );
        let bitrate = self.choices(
            "bitrate",
            &Bitrate::ALL,
            settings.bitrate,
            Bitrate::label,
            |settings, bitrate| settings.bitrate = bitrate,
            cx,
        );
        let compatibility = self.choices(
            "compatibility",
            &Compatibility::ALL,
            settings.compatibility,
            |compatibility| compatibility.label().to_string(),
            |settings, compatibility| settings.compatibility = compatibility,
            cx,
        );
        let force_hdr = self.choices(
            "force-hdr",
            &[false, true],
            settings.force_hdr_transcode,
            |on| if on { "On".into() } else { "Off".into() },
            |settings, on| settings.force_hdr_transcode = on,
            cx,
        );

        div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .px_8()
            .pb_10()
            .flex()
            .flex_col()
            .gap_8()
            .text_color(text())
            .when_some(self.error.clone(), |d, error| d.child(div().text_sm().text_color(gpui::red()).child(error)))
            .child(Self::section(
                "Resolution",
                "The largest picture size to accept. The server downscales anything bigger.",
                resolution,
            ))
            .child(Self::section(
                "Bitrate",
                "How much data per second to accept: higher means better quality and more bandwidth. Auto runs a quick speed test before each play and uses the speed it measured; Maximum (360 Mbps) is effectively no limit.",
                bitrate,
            ))
            .child(Self::section(
                "Compatibility",
                settings.compatibility.description(),
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(compatibility)
                    .when(settings.compatibility == Compatibility::Custom, |d| {
                        d.child(Self::custom_field("Containers", &self.containers))
                            .child(Self::custom_field("Video codecs", &self.video_codecs))
                            .child(Self::custom_field("Audio codecs", &self.audio_codecs))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(text_dim())
                                    .child("Comma-separated Jellyfin names; an empty list allows any."),
                            )
                    }),
            ))
            .child(Self::section(
                "Force HDR/DV transcode",
                "Converts HDR and Dolby Vision video to regular video. Use this if colors look wrong.",
                force_hdr,
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(text_dim())
                    .child("Changes are saved as you make them and apply from the next play. The player's Quality menu changes resolution and bitrate for one video only."),
            )
    }
}
