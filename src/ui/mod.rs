pub mod app;
pub mod login;
pub mod player_view;
pub mod text_input;

use gpui::{Hsla, RenderImage, rgb};
use smallvec::smallvec;
use std::sync::Arc;

// Infuse-ish dark palette.
pub fn bg() -> Hsla {
    rgb(0x0b0d12).into()
}
pub fn surface() -> Hsla {
    rgb(0x161a22).into()
}
pub fn surface_hi() -> Hsla {
    rgb(0x222834).into()
}
pub fn text() -> Hsla {
    rgb(0xf2f4f8).into()
}
pub fn text_dim() -> Hsla {
    rgb(0x9aa3b2).into()
}
pub fn accent() -> Hsla {
    rgb(0xf5a623).into()
}

/// Decode an encoded image (jpg/png/webp), downscale to `max_w`, and convert
/// to GPUI's BGRA `RenderImage`.
pub fn decode_render_image(bytes: &[u8], max_w: u32) -> anyhow::Result<Arc<RenderImage>> {
    let img = image::load_from_memory(bytes)?;
    let img = if img.width() > max_w {
        let h = u64::from(img.height()) * u64::from(max_w) / u64::from(img.width());
        let h = u32::try_from(h).unwrap_or(u32::MAX);
        img.resize_exact(max_w, h.max(1), image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut rgba = img.into_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(smallvec![image::Frame::new(rgba)])))
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_runtime(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0).round() as u64;
    if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// Append a timestamped line to proof/playback/run.log (proof mode only).
pub fn proof_log(msg: impl AsRef<str>) {
    use std::io::Write;
    if !crate::proof_mode() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let line = format!("[{ts:.3}] {}\n", msg.as_ref());
    log::info!("{}", msg.as_ref());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::proof_dir().join("run.log"))
    {
        f.write_all(line.as_bytes()).log_err();
    }
}

/// Log an error that nobody can act on, instead of silently discarding it
/// (e.g. a view released before its background task finished).
pub trait LogErr {
    /// Log the error, if any, with the caller's location.
    fn log_err(self);
}

impl<T, E: std::fmt::Debug> LogErr for Result<T, E> {
    #[track_caller]
    fn log_err(self) {
        if let Err(error) = self {
            let location = std::panic::Location::caller();
            log::warn!("{}:{}: {error:?}", location.file(), location.line());
        }
    }
}
