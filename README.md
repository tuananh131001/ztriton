> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# ztriton

A native macOS Jellyfin client built with GPUI, with playback through libmpv.

- Sign in once; the session is kept in the macOS Keychain.
- Home, detail and player screens.
- Hardware-decoded video (VideoToolbox) rendered on the GPU via IOSurfaces
  drawn with Metal.
- Subtitle picker, including sidecar files (e.g. `.srt`) stored next to the
  video on the server; the chosen subtitle is remembered.
- Playback resumes where it was left, even for watches too short for
  Jellyfin to save, and progress is reported back to the server.

## Requirements

- macOS, Rust 1.99 (`mise install`)
- mpv 0.42+ (`brew install mpv`)

## Build & run

```sh
make install   # builds dist/ztriton.app, installs to ~/Applications
make test      # unit + live-server tests
make bench     # CPU/memory/frame-rate benchmark while playing (needs a signed-in session)
make uninstall # remove the app; keeps the Keychain login and device id
```

Builds are signed with your Apple Development identity when one exists
(free Apple ID, Xcode › Settings › Accounts), so the Keychain stops asking
for your password after every rebuild. Override with `SIGN_IDENTITY`.

Live-server tests read `JELLYFIN_URL`, `JELLYFIN_USER` and `JELLYFIN_PASSWORD`
from the environment or from `.env` (git-ignored). Set it up once with
`cp .env.example .env`. Without it they skip, except the subtitle test,
which falls back to the app's saved login.

Sign in from the app. The session is stored in the macOS Keychain, and
preferences and resume positions are stored in
`~/Library/Application Support/ztriton/`.

## Player keys

| Key | Action |
| --- | --- |
| `space` | Play / pause |
| `←` `→` (`⇧` for 60s) | Seek 10s |
| `↑` `↓` | Volume |
| `m` | Mute |
| `a` | Cycle audio track |
| `s` | Cycle subtitles (or click **CC** to pick one) |
| `f` | Fullscreen |
| `esc` | Close player |

`⌃⌘F` toggles full screen anywhere in the app, as does the button in the
top-right corner.

Verification screenshots, logs and benchmark results are written to `proof/`
(git-ignored).
