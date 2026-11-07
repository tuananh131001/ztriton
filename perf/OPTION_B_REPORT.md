# Option B — mpv draws natively on Metal (gpu-next + macvk), embedded in the GPUI window

Branch `proto/option-b-native-mpv-metal`. Throwaway prototype for the
performance comparison.

## Result

* **Native rendering works.** mpv draws through gpu-next, Vulkan (MoltenVK) and
  Metal, with zero-copy VideoToolbox decoding. The video shows inside the GPUI
  window, and the GPUI controls render on top of it.
  * Fullscreen, resize, subtitles and close/reopen all work.
  * `--wid` itself is **not supported** by mpv on macOS. The prototype instead
    moves mpv's own `CAMetalLayer` view out of mpv's window and into GPUI's.
* **CPU is about 3× lower:** 13.5–17.7% during playback, against the 49.4%
  baseline.
* **No frames are lost:** 0 dropped frames, and mpv's `estimated-vf-fps`
  reads 23.98 on 23.976 fps content.
* **Memory is not lower out of the box.** It measured 563–600 MB, against the
  413 MB baseline.
  * With option A's three non-renderer changes applied (`option-b-fair`), it
    falls to **383–398 MB**.
  * What remains is mostly the demuxer cache plus mpv's swapchain and
    VideoToolbox surfaces (see below).
* **The price is fragility.** The approach relies on undocumented mpv
  internals, and on keeping every libmpv call off the main thread to avoid a
  deadlock.

## What was built

* **`--wid` doesn't work on macOS.** mpv 0.42's `macvk` context
  (`video/out/vulkan/context_mac.m` + `video/out/mac_common.swift`) never reads
  `--wid`. It always creates its own `NSWindow` (`initView` / `initWindow`) and
  sizes the swapchain from **that window's** `framePixel`, not from the view.
  The manual describes `--wid` only for X11, win32 and Android. So the brief's
  approach can't be used as-is.
* **The workaround: let mpv create its window, then take its view.**
  1. mpv runs with `vo=gpu-next gpu-api=vulkan gpu-context=macvk hwdec=videotoolbox`.
     It also gets `force-render=yes border=no focus-on=never keepaspect-window=no auto-window-resize=no`
     and `cursor-autohide=no input-cursor=no input-vo-keyboard=no`
     (`src/player.rs`, `PlayerOptions::native_video`).
  2. On `MPV_EVENT_VIDEO_RECONFIG`, the event thread reads the `window-id`
     property. On macvk this is mpv's `NSWindow*` (`VOCTRL_GET_WINDOW_ID`).
  3. On the main thread (`src/ui/native_video.rs`):
     * Find the subview of mpv's window whose layer is a `CAMetalLayer`.
     * Move that view into GPUI's content view with
       `addSubview:positioned:NSWindowBelow relativeTo:<GPUI's NSView>`.
       The GPUI view comes from `raw-window-handle`, via
       `HasWindowHandle for gpui::Window`.
     * Hide mpv's now-empty window (`orderOut:`) and make the GPUI window key again.
  4. On every GPUI render, `sync_size` copies GPUI's content rect onto mpv's
     hidden window with `setFrame:display:`. mpv's window delegate then raises
     `VO_EVENT_RESIZE`, so the swapchain follows resize and fullscreen.
     This is a no-op when the size hasn't changed.
* **How GPUI ends up on top.** GPUI 0.2.2's `CAMetalLayer` is created with
  `opaque = false` and cleared to alpha 0, so anything GPUI doesn't paint is
  see-through. The root view and the player view skip their background fill
  while the player is shown (`src/ui/app.rs`, `src/ui/player_view.rs`). The
  video then shows through, and the GPUI controls (gradients, scrubber,
  buttons, subtitle menu, OSD) draw over it. GPUI's view is the topmost
  subview, so it still receives all mouse and keyboard events.
* **A command thread for mpv calls (needed for correctness).** macvk does
  `DispatchQueue.main.sync` on the VO thread during VO init and reconfig, while
  mpv's core thread waits for the VO. A synchronous `mpv_command` or
  `mpv_set_property` from the GPUI main thread at that moment would deadlock:
  * the main thread waits for the core,
  * the core waits for the VO,
  * the VO waits for the main thread.

  So `Player::run`/`set`, including `loadfile`, now post to a `mpv-commands`
  thread. Stats come from observed properties
  (`frame-drop-count`, `decoder-frame-drop-count`, `estimated-vf-fps`), never
  from `get_property` on the UI thread.
* The software render path (`vo=libmpv`) is still there behind
  `native_video: false`, so the e2e tests in `tests/` still build. The app
  always uses the native path.
* **Changes to the stats patch and `scripts/bench.sh`.** I applied
  `stats-log.patch`, then rewrote its log line. GPUI presents no video frames in
  this option, so it now logs:
  `time-pos estimated_vf_fps vo_drops decoder_drops hwdec embedded`.
  In `scripts/bench.sh`, `presented_fps_per_video_sec` prints `n/a` when no
  `presented=` field exists, instead of a misleading `0.00`. A new field,
  `mpv_estimated_vf_fps`, averages mpv's own estimate. `dropped_frames` is
  unchanged: VO + decoder drops over the window.
* `option-b-fair` adds the three non-renderer changes from option A:
  * `demuxer-max-bytes=32MiB` and `demuxer-max-back-bytes=8MiB`;
  * the built-in `load-*` scripts and profiles, plus
    `input-builtin-drag-and-drop`, set to `no`;
  * `ZtritonApp::play` drains the poster cache and calls `drop_image` on each
    poster.

  These changes are what the committed code contains now.
* For testing only, proof mode gained `ZTRITON_PROOF_WINDOW=1` (fullscreen on
  and off, then resize to 900×520) and `ZTRITON_PROOF_REOPEN=1` (close the
  player, then reopen the same item right away).

## What works / what doesn't

Checked with `ZTRITON_PROOF=1 ZTRITON_PROOF_WINDOW=1 ZTRITON_PROOF_REOPEN=1 ZTRITON_PROOF_QUIT=1 target/release/ztriton --autoplay first`
and screenshots at each marker. The run went: autoplay → seek to 60% → pause
→ fullscreen → exit fullscreen → resize → close → reopen → the same steps
again → quit. The process exited 0, with no hang.

| Area | Status |
|---|---|
| Video in the GPUI window | ✅ The 1920×816 H.264 source plays with `hwdec-current=videotoolbox`. mpv/libplacebo renders through MoltenVK into its own `CAMetalLayer`. |
| GPUI overlay on top | ✅ The top bar, scrubber, play/pause, track chips and OSD all render over the video, see `proof/perf/option-b-player.png`. The controls auto-hide as before. |
| Mouse and keyboard | ✅ for mouse: GPUI's view is topmost, so hit-testing is normal. Keyboard: the GPUI window is made key again after mpv's `makeKey`. I did not test key presses with synthetic input, because that needs Accessibility permission. |
| Fullscreen | ✅ GPUI's `toggle_fullscreen` works. mpv's view autoresizes, and the hidden window is resized so the swapchain matches. The video letterboxes correctly. |
| Resize | ✅ Going to 900×520 gives correct letterboxing. The resize is applied on the next GPUI render. The 1 s status tick sets a ceiling on that delay; for a moment the old swapchain is scaled by `layerContentsPlacement`. |
| Subtitles | ✅ mpv/libass draws them into the same Metal frame, so they come for free. The remembered Vietnamese SRT showed after reopen, placed in the letterbox margin. |
| Close / reopen | ✅ On `stop`, mpv's VO uninit removes its view from our content view (`uninitCommon` → `view.removeFromSuperview()`). An immediate reopen while the old core is still tearing down in the background also works. |
| Quit with the player open | ✅ in proof runs. Risk: GPUI blocks the main thread on the quit futures, while mpv's teardown needs `main.sync`. This works today only because teardown runs on a background task and `stop` already removed the VO. |

## Benchmarks (M1, `scripts/bench.sh`, 30 s window)

Baseline from the brief (software render API + GPUI `RenderImage`):
49.4% CPU, 413 MB physical footprint, and the UI presented only 740 of 883 mpv frames.

```
# option-b (as built: demuxer-max-bytes=150MiB, mpv defaults for scripts, poster cache kept)
label=option-b mode=play cpu_pct=14.7 footprint_mb=578.1 footprint_peak_mb=578.2 rss_mb=166.7 energy_impact=13.8 presented_fps_per_video_sec=0.00* dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
label=option-b mode=play cpu_pct=13.5 footprint_mb=572.9 footprint_peak_mb=575.5 rss_mb=148.8 energy_impact=12.6 presented_fps_per_video_sec=0.00* dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
label=option-b mode=play cpu_pct=16.5 footprint_mb=562.9 footprint_peak_mb=563.0 rss_mb=245.6 energy_impact=15.3 presented_fps_per_video_sec=n/a dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
label=option-b mode=play cpu_pct=17.7 footprint_mb=600.3 footprint_peak_mb=600.3 rss_mb=156.2 energy_impact=16.4 presented_fps_per_video_sec=n/a dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
label=option-b mode=idle cpu_pct=0.6 footprint_mb=143.0 footprint_peak_mb=159.0 rss_mb=85.6 energy_impact=0.6 presented_fps_per_video_sec=n/a dropped_frames=n/a mpv_estimated_vf_fps=n/a window_s=30
label=option-b mode=idle cpu_pct=0.6 footprint_mb=142.1 footprint_peak_mb=158.1 rss_mb=96.3 energy_impact=0.5 presented_fps_per_video_sec=n/a dropped_frames=n/a mpv_estimated_vf_fps=n/a window_s=30

# option-b-fair (+ option A's non-renderer changes: demuxer-max-bytes=32MiB,
# demuxer-max-back-bytes=8MiB, built-in scripts/profiles off, poster cache freed when the player opens)
label=option-b-fair mode=play cpu_pct=14.6 footprint_mb=383.1 footprint_peak_mb=383.2 rss_mb=191.9 energy_impact=13.7 presented_fps_per_video_sec=n/a dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
label=option-b-fair mode=play cpu_pct=15.6 footprint_mb=398.0 footprint_peak_mb=399.6 rss_mb=226.0 energy_impact=14.6 presented_fps_per_video_sec=n/a dropped_frames=0 mpv_estimated_vf_fps=23.98 window_s=30
```

\* These two lines were recorded before I fixed the bench parser. Their
`0.00` means "no GPUI presented-frame counter", not zero fps. With option B,
GPUI presents no video frames, so `presented_fps_per_video_sec` is `n/a` and
the frame-delivery evidence is the following:

* `dropped_frames` (mpv VO + decoder drops during the window) = 0
* `mpv_estimated_vf_fps` = 23.98, on 23.976 fps content
* `hwdec-current=videotoolbox` in every stats line

The first two play runs happened while another agent's `jellyfin_e2e` binary
sat idle at a permission dialog (0% CPU). Every run held the shared
`bench.lock`.

`MODE=idle` failed in `scripts/bench.sh` under macOS bash 3.2: an empty
`"${args[@]}"` with `set -u` gives an "unbound variable" error. I fixed it
with `${args[@]+"${args[@]}"}`.

### Memory breakdown

From `footprint -p` and `vmmap -summary` of the running app, sampled 15 s
after `playback-restart`:

| Category | option-b (579 MB) | option-b-fair (395 MB) |
|---|---|---|
| Malloc Small (dirty) | 273 MB | 137 MB |
| Malloc Large | 46 MB | 41 MB |
| IOSurface | 90 MB | 90 MB |
| Owned unmapped (graphics) | 67 MB | 67 MB |
| IOAccelerator (graphics) | 61 MB | 21 MB |

* **Malloc** is most likely mpv's demuxer cache. The 150 MiB forward
  buffer plus the default back buffer fits the 136 MB drop once the cache is
  capped at 32 + 8 MiB. This is inferred from the before/after numbers, not
  from attributing individual allocations. Malloc is the largest category in both builds.
* **IOSurface (~90 MB)** is the cost of the renderer itself:
  * VideoToolbox decode surfaces;
  * mpv's swapchain. macvk *creates* its `CAMetalLayer` as **rgba16Float**
    (`MetalLayer.init`). If the swapchain keeps that format, each drawable is
    8 B/px, and three drawables at this window's 2560×1584 backing size come
    to about 97 MB. I did not confirm the format the swapchain actually
    chose; mpv's `-v` log prints it.
* **GPU objects:** "Owned unmapped (graphics)" (67 MB) holds libplacebo/MoltenVK
  textures and buffers. Freeing GPUI's poster atlas brings IOAccelerator from
  61 MB down to 21 MB.
* **Possible cuts to try (not measured):**
  * a smaller swapchain pixel format: mpv's gpu-next currently picks rgba16f
    on macvk for EDR, and an 8-bit option is needed;
  * fewer swapchain images: `--swapchain-depth` and the MoltenVK settings.

## Risks

* **It depends on mpv internals, not an API.** The prototype relies on:
  * `window-id` returning mpv's `NSWindow`,
  * the video view being a `CAMetalLayer`-backed subview of the window's content view,
  * the swapchain size coming from the window frame,
  * the view moving cleanly between windows.

  None of this is documented or stable. Any refactor of `mac_common.swift` or
  `window.swift` can break it silently, for example a black video or a wrong
  size.
* **Deadlock class.** Any synchronous libmpv call on the main thread can
  deadlock against macvk's `DispatchQueue.main.sync`. The command thread covers
  the current call sites. Watch for:
  * `Player::property_string`, which is still synchronous; nothing in the app
    calls it on the UI thread now;
  * future calls added on the UI thread;
  * blocking the main thread on mpv teardown, for example dropping `Player` on
    the main thread or GPUI's quit hook. These have to stay off the main thread.
* **mpv still owns a hidden `NSWindow` per playback.** When it is created it
  is `orderFront`ed and `makeKey`ed for a moment before we move the view out
  and hide it, so a one-frame flash of a borderless window is possible. I did
  not catch one in screenshots. mpv also calls
  `NSApp.setActivationPolicy(.regular)`. When the app isn't a bundle, it also
  sets `NSApp.applicationIconImage` to mpv's icon (`setAppIcon`), which
  affects the Dock icon of `target/release/ztriton`. A `.app` bundle avoids
  this.
* **mpv's view still receives `mouseMoved`** through its tracking area even
  though it is covered. That's harmless with `input-cursor=no` and
  `cursor-autohide=no`, but mpv's menu registration (`AppHub.shared.menu`) and
  window-level behaviour are present and untested.
* **`force-render=yes`** is needed because the hidden window is never
  "visible". mpv therefore keeps rendering when our window is occluded or
  minimised, and loses that power saving.
* **Size sync is lazy.** It is driven by GPUI renders, not
  `windowDidResize`. A live-resize drag can show a stretched frame for up to
  one render.
* HDR/EDR and the ICC profile follow mpv's hidden window's screen, not
  necessarily the screen GPUI's window is on. This is untested with
  multiple displays.
* The SW path is kept alongside the native one, so `player.rs` carries two
  code paths. The e2e tests still use the SW path. I did not run them here:
  they need the live server, and another agent was running them.
* `Player::select_subtitle` is now fire-and-forget, through the command
  thread. `tests/subtitles_e2e.rs` reads `sub-text` right after selecting, so
  it may need a short wait.
* `option-b-fair` also frees GPUI's poster cache whenever the player opens.
  Posters are fetched again when you go back.
