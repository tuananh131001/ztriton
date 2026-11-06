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
make test      # live-server tests use the saved login or JELLYFIN_URL/USER/PASSWORD
```

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
