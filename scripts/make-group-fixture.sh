#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later

# Build a folder of JPEGs for exercising photo groups.
#
#   scripts/make-group-fixture.sh <dir>                              # 40 photos, 4 bursts
#   scripts/make-group-fixture.sh <dir> --count 20000 --groups 2000  # the perf folder
#
# Photos are IMG_0001.JPG onward in capture order. Bursts of 6, 5, 4 and 3
# frames (cycling when there are more) sit between single shots. A burst's
# frames are one second apart and everything else is a minute apart, so any
# gap between two and sixty seconds separates the bursts. The files carry
# no EXIF date, so the app falls back to the mtime, which is set to the
# capture time. Every burst frame but the middle one is blurred, so the
# sharpness score has one clear winner.
#
# --groups M lays out M bursts and also writes one group sidecar per burst
# to .lightphotos/groups/g-fixtureNNNNN.json, representative the sharp frame.
# Without it there are 4 bursts and no sidecars.
#
# Rerunning converges on the same folder: unchanged files are not rewritten,
# mtimes are reset, and IMG_*.JPG or g-fixture*.json files the layout no
# longer names are removed. Nothing else in <dir> is touched. Large counts
# reuse 64 rendered scenes as APFS clones, so 20 000 photos take little disk.

set -euo pipefail

usage() {
  echo "usage: $0 <dir> [--count N] [--groups M]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
dir="$1"
shift
count=40
groups=4
sidecars=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --count) [[ $# -ge 2 ]] || usage; count="$2"; shift 2 ;;
    --groups) [[ $# -ge 2 ]] || usage; groups="$2"; sidecars=1; shift 2 ;;
    *) usage ;;
  esac
done
[[ "$count" =~ ^[0-9]+$ && "$groups" =~ ^[0-9]+$ ]] || usage

mkdir -p "$dir"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/fixture.swift" <<'SWIFT'
import CoreImage
import Foundation

let args = CommandLine.arguments
let dir = URL(fileURLWithPath: args[1])
let count = Int(args[2])!
let bursts = Int(args[3])!
let writeSidecars = args[4] == "1"

let burstSizes = (0..<bursts).map { [6, 5, 4, 3][$0 % 4] }
let singles = count - burstSizes.reduce(0, +)
guard singles >= 0 else {
    FileHandle.standardError.write("\(bursts) bursts need \(burstSizes.reduce(0, +)) photos, more than --count \(count)\n".data(using: .utf8)!)
    exit(1)
}

struct Shot {
    var scene: Int
    var frame: Int
    var blurred: Bool
    var label: String
    var seconds: Int
}

var shots: [Shot] = []
var burstMembers: [[Int]] = []
var burstReps: [Int] = []
var clock = 0
var scene = 0
func single() {
    clock += 60
    shots.append(Shot(scene: scene, frame: 0, blurred: false, label: "Single \(scene)", seconds: clock))
    scene += 1
}
let perGap = singles / (bursts + 1)
for (b, size) in burstSizes.enumerated() {
    for _ in 0..<perGap { single() }
    clock += 60
    var members: [Int] = []
    for f in 0..<size {
        if f > 0 { clock += 1 }
        members.append(shots.count)
        shots.append(Shot(scene: scene, frame: f, blurred: f != size / 2, label: "Burst \(b + 1) frame \(f + 1)/\(size)", seconds: clock))
    }
    burstMembers.append(members)
    burstReps.append(members[size / 2])
    scene += 1
}
while shots.count < count { single() }

let width = max(4, String(count).count)
func name(_ i: Int) -> String {
    let n = String(i + 1)
    return "IMG_" + String(repeating: "0", count: width - n.count) + n + ".JPG"
}

let maxRenderedScenes = count > 200 ? 64 : Int.max
struct Key: Hashable { var scene: Int; var frame: Int; var blurred: Bool }
func key(_ s: Shot) -> Key { Key(scene: s.scene % maxRenderedScenes, frame: s.frame, blurred: s.blurred) }

let context = CIContext(options: [.cacheIntermediates: false])
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!
let rect = CGRect(x: 0, y: 0, width: 1200, height: 800)

func render(_ s: Shot) -> Data {
    let hue = CGFloat((s.scene * 37) % 360) / 360
    let tint = CIColor(cgColor: NSColorLike.hsb(hue, 0.7, 0.55))
    let pale = CIColor(cgColor: NSColorLike.hsb(hue, 0.25, 0.95))
    let checker = CIFilter(name: "CICheckerboardGenerator", parameters: [
        "inputCenter": CIVector(x: CGFloat(s.frame * 7), y: CGFloat(s.frame * 3)),
        "inputColor0": tint,
        "inputColor1": pale,
        "inputWidth": 40 + (s.scene % 5) * 8,
        "inputSharpness": 1,
    ])!.outputImage!.cropped(to: rect)
    let text = CIFilter(name: "CITextImageGenerator", parameters: [
        "inputText": s.label,
        "inputFontName": "Helvetica-Bold",
        "inputFontSize": 72,
        "inputScaleFactor": 1,
    ])!.outputImage!
    let plate = CIImage(color: CIColor(red: 1, green: 1, blue: 1, alpha: 0.85))
        .cropped(to: text.extent.insetBy(dx: -24, dy: -16))
    let label = text.composited(over: plate)
        .transformed(by: CGAffineTransform(translationX: 60, y: 60))
    var image = label.composited(over: checker).cropped(to: rect)
    if s.blurred {
        image = image.clampedToExtent()
            .applyingGaussianBlur(sigma: 9)
            .cropped(to: rect)
    }
    return context.jpegRepresentation(of: image, colorSpace: srgb, options: [:])!
}

enum NSColorLike {
    static func hsb(_ h: CGFloat, _ s: CGFloat, _ v: CGFloat) -> CGColor {
        let i = Int(h * 6) % 6
        let f = h * 6 - CGFloat(Int(h * 6))
        let p = v * (1 - s), q = v * (1 - f * s), t = v * (1 - (1 - f) * s)
        let (r, g, b): (CGFloat, CGFloat, CGFloat) = [(v, t, p), (q, v, p), (p, v, t), (p, q, v), (t, p, v), (v, p, q)][i]
        return CGColor(srgbRed: r, green: g, blue: b, alpha: 1)
    }
}

let fm = FileManager.default
var calendar = Calendar(identifier: .gregorian)
calendar.timeZone = TimeZone.current
let start = calendar.date(from: DateComponents(year: 2024, month: 6, day: 1, hour: 10))!

func sameBytes(_ url: URL, _ data: Data) -> Bool {
    guard let size = (try? fm.attributesOfItem(atPath: url.path))?[.size] as? Int, size == data.count else {
        return false
    }
    return (try? Data(contentsOf: url)) == data
}

var firstOfKey: [Key: (URL, Data)] = [:]
var written = 0
for (i, s) in shots.enumerated() {
    let url = dir.appendingPathComponent(name(i))
    let k = key(s)
    if let (source, data) = firstOfKey[k] {
        if !sameBytes(url, data) {
            try? fm.removeItem(at: url)
            if clonefile(source.path, url.path, 0) != 0 {
                try data.write(to: url, options: .atomic)
            }
            written += 1
        }
    } else {
        let data = render(s)
        if !sameBytes(url, data) {
            try data.write(to: url, options: .atomic)
            written += 1
        }
        firstOfKey[k] = (url, data)
    }
    let date = start.addingTimeInterval(TimeInterval(s.seconds))
    try fm.setAttributes([.modificationDate: date], ofItemAtPath: url.path)
}

let wanted = Set((0..<count).map(name))
let isPhoto = try NSRegularExpression(pattern: "^IMG_[0-9]+\\.JPG$")
var removed = 0
for entry in try fm.contentsOfDirectory(atPath: dir.path) where !wanted.contains(entry) {
    if isPhoto.firstMatch(in: entry, range: NSRange(entry.startIndex..., in: entry)) != nil {
        try fm.removeItem(at: dir.appendingPathComponent(entry))
        removed += 1
    }
}

let groupsDir = dir.appendingPathComponent(".lightphotos/groups")
var sidecarNames = Set<String>()
if writeSidecars {
    try fm.createDirectory(at: groupsDir, withIntermediateDirectories: true)
    for (b, members) in burstMembers.enumerated() {
        let file = String(format: "g-fixture%05d.json", b + 1)
        sidecarNames.insert(file)
        let list = members.map { "\"\(name($0))\"" }.joined(separator: ",")
        let body = Data("{\"v\":1,\"members\":[\(list)],\"representative\":\"\(name(burstReps[b]))\"}\n".utf8)
        let url = groupsDir.appendingPathComponent(file)
        if !sameBytes(url, body) {
            try body.write(to: url, options: .atomic)
        }
    }
}
if let entries = try? fm.contentsOfDirectory(atPath: groupsDir.path) {
    for entry in entries where entry.hasPrefix("g-fixture") && entry.hasSuffix(".json") && !sidecarNames.contains(entry) {
        try fm.removeItem(at: groupsDir.appendingPathComponent(entry))
    }
}

print("\(dir.path): \(count) photos, \(bursts) bursts \(burstSizes.prefix(4)), \(writeSidecars ? bursts : 0) group sidecars; wrote \(written), removed \(removed)")
SWIFT

swift "$work/fixture.swift" "$dir" "$count" "$groups" "$sidecars"
