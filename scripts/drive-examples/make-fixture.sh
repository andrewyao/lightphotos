#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Each photo is a flat colour with its number drawn large, so a screenshot
# shows which photo a cell or the Loupe holds. macOS only: it draws with
# CoreGraphics through the swift interpreter the command line tools ship.
set -eu

dir=${1:?usage: make-fixture.sh <dir> [count]}
count=${2:-12}
# IMG_0001.jpg would overwrite a camera's IMG_0001.JPG on a case-insensitive
# volume, so only an empty or new folder is written.
if [ -d "$dir" ] && [ -n "$(ls -A "$dir")" ]; then
    echo "make-fixture.sh: $dir is not empty; refusing to write into it" >&2
    exit 1
fi
mkdir -p "$dir"

swift - "$dir" "$count" <<'EOF'
import Foundation
import CoreGraphics
import CoreText
import ImageIO
import UniformTypeIdentifiers

let dir = CommandLine.arguments[1]
let count = Int(CommandLine.arguments[2])!
let (w, h) = (1200, 800)
let space = CGColorSpaceCreateDeviceRGB()
for n in 1...count {
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8,
        bytesPerRow: w * 4, space: space,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let hue = CGFloat(n - 1) / CGFloat(count)
    let (r, g, b) = hsv(hue, 0.55, 0.85)
    ctx.setFillColor(CGColor(red: r, green: g, blue: b, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    let font = CTFontCreateWithName("Helvetica-Bold" as CFString, 420, nil)
    let attrs: [CFString: Any] = [
        kCTFontAttributeName: font,
        kCTForegroundColorAttributeName: CGColor(red: 1, green: 1, blue: 1, alpha: 0.95),
    ]
    let line = CTLineCreateWithAttributedString(
        CFAttributedStringCreate(nil, String(n) as CFString, attrs as CFDictionary))
    let bounds = CTLineGetBoundsWithOptions(line, .useOpticalBounds)
    ctx.textPosition = CGPoint(x: (CGFloat(w) - bounds.width) / 2 - bounds.minX,
                               y: (CGFloat(h) - bounds.height) / 2 - bounds.minY)
    CTLineDraw(line, ctx)
    let image = ctx.makeImage()!
    let path = String(format: "%@/IMG_%04d.jpg", dir, n)
    let url = URL(fileURLWithPath: path) as CFURL
    let dest = CGImageDestinationCreateWithURL(url, UTType.jpeg.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, image, [kCGImageDestinationLossyCompressionQuality: 0.8] as CFDictionary)
    CGImageDestinationFinalize(dest)
}

func hsv(_ h: CGFloat, _ s: CGFloat, _ v: CGFloat) -> (CGFloat, CGFloat, CGFloat) {
    let i = Int(h * 6)
    let f = h * 6 - CGFloat(i)
    let (p, q, t) = (v * (1 - s), v * (1 - f * s), v * (1 - (1 - f) * s))
    switch i % 6 {
    case 0: return (v, t, p)
    case 1: return (q, v, p)
    case 2: return (p, v, t)
    case 3: return (p, q, v)
    case 4: return (t, p, v)
    default: return (v, p, q)
    }
}
EOF

echo "wrote $count photos to $dir"
