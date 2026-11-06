# Changelog

## 0.0.2 — 2026-11-07

- Full screen button in the top-right corner of the player, page headers and sign-in screen.
- **⌃⌘F** now toggles full screen (it previously did nothing).
- Development builds are signed with your Apple Development identity, so macOS stops asking for the login password after every rebuild.
- Failed playback commands and UI updates are now logged instead of silently ignored.
- End-to-end tests read Jellyfin credentials from `.env`.

## 0.0.1 — 2026-11-07

First release.

- Jellyfin sign-in, kept in the macOS Keychain; home, detail and player screens.
- libmpv playback with on-screen controls and progress reporting to Jellyfin.
- Subtitle picker: click **CC** to choose a track or turn subtitles off.
- Subtitle files stored next to the video on the server (e.g. `.srt`) are loaded.
- The chosen subtitle is remembered and re-applied on the next video.
