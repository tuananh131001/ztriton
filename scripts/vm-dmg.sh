#!/usr/bin/env bash
# Build the DMG inside a Tart macOS VM and copy it to the host's dist/.
#
#   scripts/vm-dmg.sh                      # VM ztriton-build, cloned from tahoe-xcode
#   VM=other BASE=some-image scripts/vm-dmg.sh
#
# The build runs on a clean macOS rather than whatever this machine has
# accumulated. The VM is kept between runs so Rust, the libmpv build and the
# cargo target dir stay warm.
set -euo pipefail

VM=${VM:-ztriton-build}
BASE=${BASE:-tahoe-xcode}
REPO=$(cd "$(dirname "$0")/.." && pwd)
DIST="$REPO/dist"
SHARED="/Volumes/My Shared Files"

mkdir -p "$DIST"

if ! tart get "$VM" >/dev/null 2>&1; then
    echo "creating VM $VM from $BASE"
    tart clone "$BASE" "$VM"
fi

vm_state() {
    tart list --format json | python3 -c 'import json, sys; print(next((vm["State"] for vm in json.load(sys.stdin) if vm["Name"] == sys.argv[1]), ""))' "$VM"
}

started_here=false
if [ "$(vm_state)" != "running" ]; then
    echo "starting VM $VM"
    tart run "$VM" --no-graphics --dir "src:$REPO:ro" --dir "out:$DIST" >/dev/null 2>&1 &
    started_here=true
fi
stop_vm() {
    if $started_here; then
        tart stop "$VM" >/dev/null || echo "warning: could not stop VM $VM" >&2
    fi
}
trap stop_vm EXIT

tart ip "$VM" --wait 120 >/dev/null
for _ in $(seq 60); do
    tart exec "$VM" true >/dev/null 2>&1 && break
    sleep 2
done

# A VM started elsewhere may lack the shares this build reads and writes.
if ! tart exec "$VM" test -d "$SHARED/src" -a -d "$SHARED/out"; then
    echo "error: VM $VM is running without the src/out shares; stop it (tart stop $VM) and rerun" >&2
    exit 1
fi

tart exec "$VM" zsh -lc "$(cat <<EOF
set -euo pipefail
mkdir -p ~/ztriton
rsync -a --delete --exclude /target --exclude /dist --exclude /proof --exclude /.env "$SHARED/src/" ~/ztriton/
cd ~/ztriton
mise trust --quiet
mise install
mise exec -- make dmg
cp dist/ztriton-*.dmg "$SHARED/out/"
EOF
)"

echo "dmg in $DIST:"
ls -1t "$DIST"/ztriton-*.dmg | head -1
