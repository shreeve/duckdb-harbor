// Renders the DuckTable app icon at every macOS size and writes an iconset.
// A duck riding on table rows: flat, two hues, drawn from primitives so the
// mark is reproducible from source rather than a binary someone lost.
// Usage: swift assets/render-icon.swift <out-dir>

import AppKit

let outDir = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "assets/AppIcon.iconset"
try? FileManager.default.createDirectory(atPath: outDir, withIntermediateDirectories: true)

func draw(_ size: CGFloat) -> NSImage {
    let image = NSImage(size: NSSize(width: size, height: size))
    image.lockFocus()
    let s = size

    // Background: the midnight-harbor squircle, inset the way Big Sur icons are.
    let inset = s * 0.083
    let rect = NSRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let radius = rect.width * 0.225
    let bg = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
    let top = NSColor(calibratedRed: 0.10, green: 0.16, blue: 0.28, alpha: 1)
    let bottom = NSColor(calibratedRed: 0.05, green: 0.08, blue: 0.14, alpha: 1)
    NSGradient(starting: top, ending: bottom)?.draw(in: bg, angle: -90)

    // Table rows: three rounded bars, the water the duck sits on.
    let rowColor = NSColor(calibratedRed: 0.20, green: 0.30, blue: 0.46, alpha: 1)
    let rowW = rect.width * 0.64
    let rowH = rect.height * 0.055
    let rowX = rect.midX - rowW / 2
    for i in 0..<3 {
        let y = rect.minY + rect.height * (0.16 + 0.115 * CGFloat(i))
        let bar = NSBezierPath(
            roundedRect: NSRect(x: rowX, y: y, width: rowW, height: rowH),
            xRadius: rowH / 2, yRadius: rowH / 2
        )
        rowColor.withAlphaComponent(1.0 - 0.28 * CGFloat(i)).setFill()
        bar.fill()
    }

    // Duck: body ellipse and head circle in duck-yellow, beak wedge, eye.
    let yellow = NSColor(calibratedRed: 1.00, green: 0.79, blue: 0.20, alpha: 1)
    let cx = rect.midX - rect.width * 0.04
    let waterY = rect.minY + rect.height * 0.16 + 0.115 * 2 * rect.height + rowH
    let body = NSBezierPath(ovalIn: NSRect(
        x: cx - rect.width * 0.26,
        y: waterY - rect.height * 0.02,
        width: rect.width * 0.52,
        height: rect.height * 0.30
    ))
    yellow.setFill()
    body.fill()

    let headR = rect.width * 0.14
    let headCX = cx + rect.width * 0.17
    let headCY = waterY + rect.height * 0.33
    let head = NSBezierPath(ovalIn: NSRect(
        x: headCX - headR, y: headCY - headR, width: headR * 2, height: headR * 2
    ))
    yellow.setFill()
    head.fill()

    // Neck: joins head and body so the silhouette reads as one shape.
    let neck = NSBezierPath(
        roundedRect: NSRect(
            x: headCX - headR * 0.85,
            y: waterY + rect.height * 0.08,
            width: headR * 1.7,
            height: rect.height * 0.25
        ),
        xRadius: headR * 0.8, yRadius: headR * 0.8
    )
    yellow.setFill()
    neck.fill()

    // Beak: a rounded wedge pointing right.
    let beak = NSBezierPath()
    let beakY = headCY - headR * 0.05
    beak.move(to: NSPoint(x: headCX + headR * 0.7, y: beakY + headR * 0.35))
    beak.line(to: NSPoint(x: headCX + headR * 1.75, y: beakY + headR * 0.02))
    beak.line(to: NSPoint(x: headCX + headR * 0.7, y: beakY - headR * 0.32))
    beak.close()
    NSColor(calibratedRed: 0.95, green: 0.52, blue: 0.13, alpha: 1).setFill()
    beak.fill()

    // Eye.
    let eyeR = headR * 0.16
    let eye = NSBezierPath(ovalIn: NSRect(
        x: headCX + headR * 0.18 - eyeR,
        y: headCY + headR * 0.28 - eyeR,
        width: eyeR * 2, height: eyeR * 2
    ))
    bottom.setFill()
    eye.fill()

    // Wing: a subtle darker ellipse on the body.
    let wing = NSBezierPath(ovalIn: NSRect(
        x: cx - rect.width * 0.17,
        y: waterY + rect.height * 0.045,
        width: rect.width * 0.24,
        height: rect.height * 0.15
    ))
    NSColor(calibratedRed: 0.93, green: 0.68, blue: 0.10, alpha: 1).setFill()
    wing.fill()

    image.unlockFocus()
    return image
}

func writePNG(_ image: NSImage, px: Int, name: String) {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    )!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    image.draw(in: NSRect(x: 0, y: 0, width: px, height: px))
    NSGraphicsContext.restoreGraphicsState()
    let data = rep.representation(using: .png, properties: [:])!
    try! data.write(to: URL(fileURLWithPath: "\(outDir)/\(name).png"))
}

for (points, scales) in [(16, [1, 2]), (32, [1, 2]), (128, [1, 2]), (256, [1, 2]), (512, [1, 2])] {
    for scale in scales {
        let px = points * scale
        let name = scale == 1 ? "icon_\(points)x\(points)" : "icon_\(points)x\(points)@2x"
        writePNG(draw(CGFloat(px)), px: px, name: name)
    }
}
print("iconset written to \(outDir)")
