#!/usr/bin/env bash
# Build libmpv and the libraries it needs from source for an older macOS, so
# the app bundle runs on every macOS from MACOS_MIN up. Homebrew bottles are
# built for the OS they were compiled on (macOS 26 bottles need macOS 26), so
# the bundled copies cannot come from Homebrew.
#
#   scripts/build-libmpv.sh [prefix]     # default prefix: target/libmpv
#
# Sources are pinned by version and SHA-256 (the versions Homebrew ships).
# Only what ztriton uses is enabled: VideoToolbox decoding, the OpenGL render
# API, libass subtitles with CoreText fonts, LuaJIT (mpv's built-in script
# options only exist with Lua) and dav1d for software AV1.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
PREFIX=$(mkdir -p "${1:-$REPO/target/libmpv}" && cd "${1:-$REPO/target/libmpv}" && pwd)
WORK="$REPO/target/libmpv-build"
MACOS_MIN=${MACOS_MIN:-13.0}
JOBS=$(sysctl -n hw.ncpu)

export MACOSX_DEPLOYMENT_TARGET=$MACOS_MIN
export CFLAGS="-mmacosx-version-min=$MACOS_MIN -O2"
export CXXFLAGS=$CFLAGS
export OBJCFLAGS=$CFLAGS
export LDFLAGS="-mmacosx-version-min=$MACOS_MIN -Wl,-headerpad_max_install_names"
# Only the libraries built here may satisfy a dependency, never Homebrew's.
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
unset PKG_CONFIG_PATH

mkdir -p "$WORK/sources"

python3 -m venv "$WORK/venv"
"$WORK/venv/bin/pip" install --quiet --disable-pip-version-check \
    meson==1.9.1 ninja==1.13.0 pkgconf==2.4.3.post2 jinja2==3.1.6 glad2==2.0.8
export PATH="$WORK/venv/bin:$PATH"
export PKG_CONFIG="$WORK/venv/bin/pkgconf"

# fetch <name> <url> <sha256>: download and unpack into $WORK/<name>.
fetch() {
    local name=$1 url=$2 sha256=$3 archive
    archive="$WORK/sources/$(basename "$url")"
    if [ ! -e "$archive" ] || ! echo "$sha256  $archive" | shasum -a 256 -c --status; then
        curl --fail --location --silent --show-error --output "$archive" "$url"
    fi
    if ! echo "$sha256  $archive" | shasum -a 256 -c --status; then
        echo "error: checksum mismatch for $url" >&2
        exit 1
    fi
    rm -rf "${WORK:?}/$name"
    mkdir -p "$WORK/$name"
    tar -xf "$archive" -C "$WORK/$name" --strip-components 1
}

meson_build() {
    local name=$1
    shift
    meson setup "$WORK/$name/build" "$WORK/$name" --prefix "$PREFIX" --libdir lib \
        --buildtype release --default-library shared -Db_ndebug=true "$@"
    meson install -C "$WORK/$name/build"
}

stamp() { echo "$PREFIX/.built-$1"; }

step() {
    local name=$1
    shift
    if [ -e "$(stamp "$name")" ]; then
        return
    fi
    echo "==> $name"
    "$@"
    touch "$(stamp "$name")"
}

build_dav1d() {
    fetch dav1d https://code.videolan.org/videolan/dav1d/-/archive/1.5.4/dav1d-1.5.4.tar.bz2 \
        2abfb0c89212e6e4733a54e0ae509ec00a5b845a6360946f918806e14aedb011
    meson_build dav1d -Denable_tools=false -Denable_tests=false
}

build_freetype() {
    fetch freetype https://downloads.sourceforge.net/project/freetype/freetype2/2.15.0/freetype-2.15.0.tar.xz \
        93cd47eb719c325107f2d3cf087d06299e944c3b48a40b55537302304dea1dd5
    meson_build freetype -Dzlib=system -Dpng=disabled -Dbrotli=disabled -Dbzip2=disabled -Dharfbuzz=disabled
}

build_fribidi() {
    fetch fribidi https://github.com/fribidi/fribidi/releases/download/v1.0.18/fribidi-1.0.18.tar.xz \
        1de5e8073d035388b71d0b2d452b1da1f8caeea8a7702997b401fe1b6250727f
    meson_build fribidi -Ddocs=false -Dbin=false -Dtests=false
}

build_harfbuzz() {
    fetch harfbuzz https://github.com/harfbuzz/harfbuzz/releases/download/15.1.0/harfbuzz-15.1.0.tar.xz \
        0fc7c565443a5f4be7da65ac5ee5bd737d9b158dc908708b7689a0a5796b0c8d
    meson_build harfbuzz -Dfreetype=enabled -Dglib=disabled -Dgobject=disabled -Dcairo=disabled \
        -Dicu=disabled -Dchafa=disabled -Dtests=disabled -Ddocs=disabled -Dutilities=disabled -Dintrospection=disabled
}

build_libass() {
    fetch libass https://github.com/libass/libass/releases/download/0.17.5/libass-0.17.5.tar.xz \
        2dca25c0e0c837ddf00b52011b3f82cac1e4ddd3ad018227806b0c2288864acc
    meson_build libass -Dfontconfig=disabled -Dcoretext=enabled -Dlibunibreak=disabled \
        -Dtest=disabled -Dcompare=disabled -Dprofile=disabled -Dfuzz=disabled -Dcheckasm=disabled
}

build_ffmpeg() {
    fetch ffmpeg https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz \
        8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
    (
        cd "$WORK/ffmpeg"
        ./configure --prefix="$PREFIX" --cc=clang --pkg-config="$PKG_CONFIG" \
            --extra-cflags="$CFLAGS" --extra-ldflags="$LDFLAGS" \
            --enable-shared --disable-static --disable-programs --disable-doc --disable-debug \
            --disable-indevs --disable-outdevs --disable-metal \
            --enable-videotoolbox --enable-audiotoolbox --enable-securetransport --enable-libdav1d
        make -j"$JOBS"
        make install
    )
}

build_libplacebo() {
    fetch libplacebo https://code.videolan.org/videolan/libplacebo/-/archive/v7.387.0/libplacebo-v7.387.0.tar.bz2 \
        14ceeeb14d1c08fcdb1a782a06129da11f842c6fddbf3a43c1c4061406ad03d2
    fetch fast_float https://github.com/fastfloat/fast_float/archive/refs/tags/v8.3.3.tar.gz \
        4dd695f051893c8218faf2ed9871fa4a4990aa6aee2d3190ed7bfa2e83092245
    fetch vulkan-headers https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/vulkan-sdk-1.4.363.0.tar.gz \
        4a078be12bef21cfebc09d878b77a63cff9d68f899254a0b00d0e37ef73e7f7e
    # The source archive leaves out its git submodules; glad and jinja come
    # from the venv, and fast_float and Vulkan-Headers are header-only (the
    # Vulkan stubs need the headers even with Vulkan disabled).
    rm -rf "$WORK/libplacebo/3rdparty/fast_float" "$WORK/libplacebo/3rdparty/Vulkan-Headers"
    ln -s "$WORK/fast_float" "$WORK/libplacebo/3rdparty/fast_float"
    ln -s "$WORK/vulkan-headers" "$WORK/libplacebo/3rdparty/Vulkan-Headers"
    meson_build libplacebo -Dvulkan=disabled -Dopengl=enabled -Dd3d11=disabled -Dshaderc=disabled \
        -Dglslang=disabled -Dlcms=disabled -Ddovi=enabled -Dlibdovi=disabled -Dunwind=disabled \
        -Dxxhash=disabled -Ddemos=false -Dtests=false
}

build_luajit() {
    fetch luajit https://github.com/LuaJIT/LuaJIT/archive/63c8f1a9b479ba5518cf6a170ff7729178bc3cfd.tar.gz \
        8cad76f444b76001b7123dc626c5d0bfc24f1f790333737443345f7e6b41bb5f
    (
        cd "$WORK/luajit"
        # LuaJIT reads MACOSX_DEPLOYMENT_TARGET itself and rejects foreign CFLAGS.
        env -u CFLAGS -u LDFLAGS make -j"$JOBS" amalg PREFIX="$PREFIX"
        env -u CFLAGS -u LDFLAGS make install PREFIX="$PREFIX"
    )
}

build_mpv() {
    fetch mpv https://github.com/mpv-player/mpv/archive/refs/tags/v0.42.0.tar.gz \
        bcfebbfa2b65a6d88dc97ea18117831072373e38b5785a2afb874ed4df5070dd
    # MetalLayer is only compiled with Vulkan, but view.swift uses it
    # unconditionally, so a Vulkan-less build fails without this guard.
    perl -0pi -e 's/(        if let metalLayer = layer as\? MetalLayer \{\n.*?\n        \}\n)/#if HAVE_VULKAN\n$1#endif\n/s' \
        "$WORK/mpv/video/out/mac/view.swift"
    grep -q '^#if HAVE_VULKAN' "$WORK/mpv/video/out/mac/view.swift" \
        || { echo "error: could not patch mpv's view.swift" >&2; exit 1; }
    meson_build mpv -Dlibmpv=true -Dcplayer=false -Dlua=luajit -Djavascript=disabled \
        -Dgl=enabled -Dgl-cocoa=enabled -Dvideotoolbox-gl=enabled -Dcocoa=enabled \
        -Dvulkan=disabled -Dshaderc=disabled -Dspirv-cross=disabled \
        -Dswift-build=enabled -Dswift-flags="-target $(uname -m)-apple-macos$MACOS_MIN" \
        -Dmacos-cocoa-cb=disabled -Dmacos-media-player=disabled -Dmacos-touchbar=disabled \
        -Dlibarchive=disabled -Dlibbluray=disabled -Ddvdnav=disabled -Dcdda=disabled \
        -Drubberband=disabled -Dzimg=disabled -Dlcms2=disabled -Duchardet=disabled \
        -Djpeg=disabled -Dmanpage-build=disabled -Dhtml-build=disabled -Dpdf-build=disabled
}

step dav1d build_dav1d
step freetype build_freetype
step fribidi build_fribidi
step harfbuzz build_harfbuzz
step libass build_libass
step ffmpeg build_ffmpeg
step libplacebo build_libplacebo
step luajit build_luajit
step mpv build_mpv

echo "libmpv for macOS $MACOS_MIN+ in $PREFIX/lib"
