#!/bin/bash
# Black-box resume proof: real app binary, real keyboard input (System Events), real Cmd-Q,
# real relaunch, live Jellyfin server. Nothing in the app is scripted or mocked.
# usage: resume_proof.sh <binary> <label>
set -u
BIN=$1; LABEL=$2
cd /Users/newtuananh/Documents/Projects/ztriton
OUT=proof/resume/$LABEL; mkdir -p $OUT; rm -f $OUT/*
ID=3455aa84003ec4dcb608f1a52320f7da   # "2012" (158 min; Jellyfin forgets stops under 5% = 7.9 min)
export RUST_LOG="info,ztriton::stats=debug,naga=warn,blade_graphics=warn" ZTRITON_MUTE=1
STORE="$HOME/Library/Application Support/ztriton/resume.json"
log() { echo "[$(date -u +%H:%M:%S)Z] $*" | tee -a $OUT/proof.log; }
shot() { screencapture -x -R60,35,1280,790 "$OUT/$1.png"; log "screenshot $1.png"; }
front() { osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $1) to true"; }
key() { osascript -e "tell application \"System Events\" to $1"; }
pos() { grep -o 'time-pos=[0-9.]*' "$1" | tail -1 | cut -d= -f2; }
store() { log "saved on this Mac: $(tr -d ' \n' < "$STORE" 2>/dev/null || echo '<no file>')"; }
jf() { # jf <method> <path> [json]
  set -a; . ./.env; set +a
  local H='MediaBrowser Client="ztriton-probe", Device="macOS", DeviceId="ztriton-probe", Version="0"'
  local AUTH=$(curl -s -X POST "$JELLYFIN_URL/Users/AuthenticateByName" -H "Authorization: $H" -H 'Content-Type: application/json' -d "{\"Username\":\"$JELLYFIN_USER\",\"Pw\":\"$JELLYFIN_PASSWORD\"}")
  local T=$(echo "$AUTH" | python3 -c 'import sys,json;print(json.load(sys.stdin)["AccessToken"])'); local U=$(echo "$AUTH" | python3 -c 'import sys,json;print(json.load(sys.stdin)["User"]["Id"])')
  curl -s -X "$1" "$JELLYFIN_URL${2//\{U\}/$U}" -H "Authorization: $H, Token=\"$T\"" -H 'Content-Type: application/json' ${3:+-d "$3"}
  curl -s -o /dev/null -X POST "$JELLYFIN_URL/Sessions/Logout" -H "Authorization: $H, Token=\"$T\""
}
server() { jf GET "/Items/$ID?userId={U}" | python3 -c 'import sys,json;d=json.load(sys.stdin)["UserData"];print("server UserData: PlaybackPositionTicks=%d (%.2fs) LastPlayedDate=%s" % (d["PlaybackPositionTicks"], d["PlaybackPositionTicks"]/1e7, d.get("LastPlayedDate")))' | tee -a $OUT/proof.log; }
launch() { "$BIN" --autoplay 2012 > "$OUT/$1.log" 2>&1 & echo $!; }
wait_for() { for _ in $(seq 1 240); do grep -q "$2" "$1" && return 0; sleep 0.25; done; log "TIMEOUT waiting for '$2' in $1"; return 1; }
quit_app() { key 'keystroke "q" using command down'; log "pressed Cmd-Q"; for _ in $(seq 1 40); do kill -0 $1 2>/dev/null || break; sleep 0.25; done; if kill -0 $1 2>/dev/null; then log "app still running, killing"; kill $1; else log "app process $1 exited"; fi; }
detail_shot() { # keep a frame of the detail screen from ~1.5 s before autoplay presses Play
  local i=0; until grep -q "start-file" $OUT/$1.log; do screencapture -x -R60,35,1280,790 "$OUT/d$((i%4)).png"; i=$((i+1)); sleep 0.5; [ $i -gt 200 ] && break; done
  mv "$OUT/d$(((i+1)%4)).png" "$OUT/$1_detail_screen.png"; rm -f $OUT/d?.png; log "screenshot $1_detail_screen.png"
}
started() { wait_for $OUT/$1.log "playback-restart" && log "mpv: $(grep -o 'playback-restart at.*' $OUT/$1.log | head -1)"; grep -o "playing '2012'.*" $OUT/$1.log | while read -r l; do log "app: $l"; done; }

log "=== $LABEL: binary $(basename $BIN), sha256 $(shasum -a 256 $BIN | cut -c1-16) ==="
log "reset: server position for 2012 -> 0, no position saved on this Mac"
jf POST /Sessions/Playing "{\"ItemId\":\"$ID\",\"PositionTicks\":0}" >/dev/null; jf POST /Sessions/Playing/Stopped "{\"ItemId\":\"$ID\",\"PositionTicks\":0}" >/dev/null
rm -f "$STORE"; sleep 1; server

log "=== RUN 1: launch, play 2012, seek to ~2:17 (under Jellyfin's 5%), pause, Cmd-Q ==="
P=$(launch run1); started run1
sleep 2; front $P; sleep 1
key 'key code 124 using shift down'; sleep 1.5; key 'key code 124 using shift down'; log "pressed shift-right x2 (+60 s each)"
sleep 4; key 'key code 49'; log "pressed space (pause)"; sleep 3
log "RUN 1 mpv time-pos at quit = $(pos $OUT/run1.log)s"
shot run1_paused_before_quit
quit_app $P; store; sleep 2; server

log "=== RUN 2: relaunch -> must resume at RUN 1's position; then watch on to ~3:2x and close with Esc ==="
P=$(launch run2); front $P; detail_shot run2; started run2
sleep 1.5; log "RUN 2 first mpv time-pos samples: $(grep -o 'time-pos=[0-9.]*' $OUT/run2.log | head -3 | tr '\n' ' ')"
front $P; sleep 0.3; key 'key code 49'; log "pressed space (pause)"; sleep 3
shot run2_resumed_paused
log "RUN 2 mpv time-pos after resume = $(pos $OUT/run2.log)s"
key 'key code 124 using shift down'; log "pressed shift-right (+60 s)"; sleep 3
log "RUN 2 mpv time-pos before Esc = $(pos $OUT/run2.log)s"
key 'key code 53'; log "pressed Esc (close player -> Detail screen)"; sleep 0.4
shot run2_detail_after_esc
store
quit_app $P; sleep 2; server

log "=== RUN 3: relaunch and Cmd-Q the moment mpv starts loading, before any frame ==="
P=$(launch run3); front $P
wait_for $OUT/run3.log "start-file" && quit_app $P
if grep -q "playback-restart" $OUT/run3.log; then log "RUN 3: NOTE a frame was rendered before quit"; else log "RUN 3: no playback-restart in log -> quit before the first frame"; fi
grep -o "playing '2012'.*" $OUT/run3.log | while read -r l; do log "app: $l"; done
store; sleep 2; server

log "=== RUN 4: relaunch -> must still resume where RUN 2 was closed ==="
P=$(launch run4); front $P; detail_shot run4; started run4
sleep 1.5; log "RUN 4 first mpv time-pos samples: $(grep -o 'time-pos=[0-9.]*' $OUT/run4.log | head -3 | tr '\n' ' ')"
front $P; sleep 0.3; key 'key code 49'; log "pressed space (pause)"; sleep 2
shot run4_resumed_paused
quit_app $P
log "done"
