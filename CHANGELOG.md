# Changelog

## 0.0.7 — 2026-11-15

- Search movies, shows and episodes across all libraries: press ⌘F, click **Search** on Home or a library, or choose ztriton › Search…. Results appear as you type, grouped into Movies, Shows and Episodes, with filters for each group. Back from a result returns to the same search.

## 0.0.6 — 2026-11-15

- Previous / next episode in the player: ⏮ ⏭ buttons beside the seek controls, or `p` / `n`. The episode being left reports its stop and keeps its resume position; Esc then returns to the new episode's page.

## 0.0.5 — 2026-11-14

- The app runs on macOS 13 Ventura and later without mpv installed: `make app` builds libmpv and its dependencies from source for macOS 13 and bundles them.
- App icon.
- Video plays on Macs without an accelerated OpenGL renderer, such as macOS virtual machines, through Apple's software renderer instead of failing with “CGLChoosePixelFormat failed”.
- `make vm-dmg` builds the DMG inside a Tart macOS VM.

## 0.0.4 — 2026-11-12

- Playback resumes where it was left, even after short watches that Jellyfin would not record (under 5% of the runtime) or when quitting with ⌘Q. Positions are also kept on this Mac, and the newer of the local and server positions wins.
- `make dmg` builds `dist/ztriton-<version>.dmg` for releases.

## 0.0.3 — 2026-11-07

- Video now stays on the GPU: mpv decodes with VideoToolbox and renders through OpenGL into IOSurfaces that the UI draws with Metal. Playback CPU use drops from about 46% to about 19% of a core and memory from about 450 MB to about 280 MB (Apple M1, 1080p H.264), still at full frame rate.
- Posters and backdrops are freed while a video plays, mpv's unused built-in scripts are no longer loaded, and the network cache is capped at 32 MiB ahead / 8 MiB behind.
- `make bench` measures CPU, memory and frame rate during playback (`scripts/bench.sh`).

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
