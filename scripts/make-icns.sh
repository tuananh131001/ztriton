#!/usr/bin/env bash
# Render the SVG app icon into an .icns with every size macOS asks for.
#
#   scripts/make-icns.sh resources/ztriton-icon.svg target/ztriton.icns
#
# AppKit renders SVG natively (macOS 13+), so this needs only the Xcode
# command line tools' swift and iconutil.
set -euo pipefail

SVG=$1
ICNS=$2
ICONSET="$(mktemp -d)/icon.iconset"
mkdir -p "$ICONSET"
trap 'rm -rf "$(dirname "$ICONSET")"' EXIT

swift - "$SVG" "$ICONSET" <<'EOF'
import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 3, let image = NSImage(contentsOf: URL(fileURLWithPath: arguments[1])) else {
    FileHandle.standardError.write("error: cannot load the SVG icon\n".data(using: .utf8) ?? Data())
    exit(1)
}
for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = points * scale
        guard let bitmap = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
            samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
            bytesPerRow: 0, bitsPerPixel: 0
        ) else { exit(1) }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
        NSGraphicsContext.current?.imageInterpolation = .high
        image.draw(in: NSRect(x: 0, y: 0, width: pixels, height: pixels))
        NSGraphicsContext.restoreGraphicsState()
        let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        guard let png = bitmap.representation(using: .png, properties: [:]) else { exit(1) }
        do {
            try png.write(to: URL(fileURLWithPath: arguments[2]).appendingPathComponent(name))
        } catch {
            FileHandle.standardError.write("error: \(error)\n".data(using: .utf8) ?? Data())
            exit(1)
        }
    }
}
EOF

mkdir -p "$(dirname "$ICNS")"
iconutil --convert icns --output "$ICNS" "$ICONSET"
