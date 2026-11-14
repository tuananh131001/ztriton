#!/usr/bin/env bash
# Copy every non-system dylib the app binary needs (libmpv and its
# dependency tree) into the bundle's Contents/Frameworks and rewrite the load
# commands to point there, so the app runs on Macs without Homebrew mpv.
#
#   scripts/bundle-dylibs.sh dist/ztriton.app/Contents/MacOS/ztriton
#
# It also raises LSMinimumSystemVersion in the bundle's Info.plist (which must
# already exist) to the newest macOS any bundled binary was built for (Homebrew
# bottles, for example, target the OS they were built on): an older macOS
# should refuse the app up front rather than crash in dyld on a missing system
# library.
#
# Signing is left to the caller: install_name_tool invalidates signatures, so
# the dylibs and the app must be (re-)signed after this runs.
set -euo pipefail

BINARY=$1
FRAMEWORKS="$(cd "$(dirname "$BINARY")/.." && pwd)/Frameworks"
rm -rf "$FRAMEWORKS"
mkdir -p "$FRAMEWORKS"

is_system() {
    case "$1" in
        /System/* | /usr/lib/*) return 0 ;;
        *) return 1 ;;
    esac
}

# Load commands of a Mach-O file, excluding a dylib's own install name.
dependencies() {
    local own_id
    own_id=$(otool -D "$1" | sed -n '2p')
    otool -L "$1" | awk 'NR > 1 { print $1 }' | while read -r dependency; do
        [ "$dependency" = "$own_id" ] || echo "$dependency"
    done
}

rpaths() {
    otool -l "$1" | awk '/cmd LC_RPATH/ { getline; getline; print $2 }'
}

# Resolve a load command of `original` (the file's path outside the bundle,
# which is what @loader_path and @rpath refer to) to a real file.
resolve() {
    local original=$1 dependency=$2 loader_dir candidate rpath
    loader_dir=$(dirname "$original")
    case "$dependency" in
        @loader_path/* | @executable_path/*)
            candidate="$loader_dir/${dependency#*/}"
            ;;
        @rpath/*)
            while read -r rpath; do
                rpath=${rpath/@loader_path/$loader_dir}
                rpath=${rpath/@executable_path/$loader_dir}
                if [ -e "$rpath/${dependency#@rpath/}" ]; then
                    candidate="$rpath/${dependency#@rpath/}"
                    break
                fi
            done < <(rpaths "$original")
            ;;
        *)
            candidate=$dependency
            ;;
    esac
    if [ -z "${candidate:-}" ] || [ ! -e "$candidate" ]; then
        echo "error: cannot resolve $dependency (needed by $original)" >&2
        return 1
    fi
    realpath "$candidate"
}

# Rewrite the non-system load commands of `target` (a copy inside the bundle)
# to `prefix/<name>`, copying each dependency in on first sight and recursing.
relink() {
    local target=$1 original=$2 prefix=$3 dependency name source
    while read -r dependency; do
        is_system "$dependency" && continue
        name=$(basename "$dependency")
        if [ ! -e "$FRAMEWORKS/$name" ]; then
            source=$(resolve "$original" "$dependency")
            cp "$source" "$FRAMEWORKS/$name"
            chmod u+w "$FRAMEWORKS/$name"
            install_name_tool -id "@rpath/$name" "$FRAMEWORKS/$name" 2>/dev/null
            relink "$FRAMEWORKS/$name" "$source" "@loader_path"
        fi
        install_name_tool -change "$dependency" "$prefix/$name" "$target" 2>/dev/null
    done < <(dependencies "$target")
}

# The binary was linked against absolute dylib paths, so its own load
# commands are already resolvable as-is.
relink "$BINARY" "$BINARY" "@executable_path/../Frameworks"

# Build-machine rpaths (Homebrew, the libmpv build prefix, the toolchain's
# Swift directory) would let the bundle silently fall back to a local install,
# hiding a missing dylib; drop them so only the bundled copies can load.
for file in "$BINARY" "$FRAMEWORKS"/*.dylib; do
    while read -r rpath; do
        case "$rpath" in
            @* | /usr/lib/* | /System/*) ;;
            *) install_name_tool -delete_rpath "$rpath" "$file" 2>/dev/null ;;
        esac
    done < <(rpaths "$file")
done

leftover=$(for file in "$BINARY" "$FRAMEWORKS"/*.dylib; do
    otool -L "$file" | awk 'NR > 1 { print $1 }' | grep -v -e '^@' -e '^/usr/lib/' -e '^/System/' || true
    rpaths "$file" | grep -v -e '^@' -e '^/usr/lib/' -e '^/System/' || true
done)
if [ -n "$leftover" ]; then
    echo "error: bundle still references libraries outside it:" >&2
    echo "$leftover" | sort -u >&2
    exit 1
fi

minimum_macos=$(for file in "$BINARY" "$FRAMEWORKS"/*.dylib; do
    otool -l "$file" | awk '/cmd LC_BUILD_VERSION/ { build = 1 } build && $1 == "minos" { print $2; exit }'
done | sort -V | tail -1)
if [ -z "$minimum_macos" ]; then
    echo "error: could not read the minimum macOS version of the bundled binaries" >&2
    exit 1
fi
plutil -replace LSMinimumSystemVersion -string "$minimum_macos" "$(dirname "$BINARY")/../Info.plist"

echo "bundled $(ls "$FRAMEWORKS" | wc -l | tr -d ' ') dylibs ($(du -sh "$FRAMEWORKS" | cut -f1)) into $FRAMEWORKS; requires macOS $minimum_macos+"
