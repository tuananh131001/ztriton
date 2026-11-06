# Changelog

## Unreleased

### Added
- Subtitle picker: click **CC** to choose a subtitle track or turn subtitles off.
- The chosen subtitle is remembered and re-applied on the next video (same track title, else same language).
- External subtitle files from Jellyfin (e.g. `.srt` stored next to the video) are now loaded.

### Fixed
- No subtitles for videos whose subtitles are separate files (e.g. Modern Family S01E09).
- The `s` key's on-screen message showed the previous subtitle track instead of the new one.

## 0.1.0

- First version: Jellyfin sign-in saved in the Keychain, home and detail screens, libmpv playback with on-screen controls and progress reporting to Jellyfin.
