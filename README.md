> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# ztriton

A native macOS Jellyfin client built with GPUI, with playback through libmpv.

## Requirements

- macOS, Rust 1.99 (`mise install`)
- mpv 0.42+ (`brew install mpv`)

## Build & run

```sh
make install   # builds dist/ztriton.app, installs to ~/Applications
make test      # unit + live-server tests
```

Live-server tests read `JELLYFIN_URL`, `JELLYFIN_USER` and `JELLYFIN_PASSWORD`
from the environment or from `.env` (git-ignored). Set it up once with
`cp .env.example .env`. Without it they skip, except the subtitle test,
which falls back to the app's saved login.

Sign in from the app. The session is stored in the macOS Keychain, and
preferences are stored in `~/Library/Application Support/ztriton/`.

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

Verification screenshots and logs live in `proof/`.
