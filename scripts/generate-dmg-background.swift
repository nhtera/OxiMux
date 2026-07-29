// Generate assets/dmg-background.tiff — the backdrop for the release DMG's
// Finder window (see scripts/make-dmg.sh for the icon layout that must stay
// in sync with the arrow drawn here).
//
// Run:  swift scripts/generate-dmg-background.swift
//
// Renders the 660x400pt canvas at 1x and 2x and fuses them into one HiDPI
// TIFF via `tiffutil -cathidpicheck`, so the backdrop stays crisp on retina
// displays. The palette is light and appearance-neutral: Finder draws icon
// labels adaptively (black in light mode), and a dark backdrop would make
// them unreadable there.
//
// Layout contract (window is 660x400, icon size 128, coordinates are icon
// CENTERS in create-dmg's top-left origin):
//   OxiMux.app icon      at (165, 190)
//   Applications alias   at (495, 190)
//   dashed guide arrow   between them, centered at (330, 190)

import AppKit

let widthPt: CGFloat = 660
let heightPt: CGFloat = 400

func render(scale: CGFloat, to url: URL) {
    let pxW = Int(widthPt * scale)
    let pxH = Int(heightPt * scale)
    guard
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: pxW, pixelsHigh: pxH,
            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
        )
    else { fatalError("could not allocate bitmap") }
    // Stamp the point size so Finder (and tiffutil) treat the 2x render as
    // 144dpi rather than a larger 72dpi image.
    rep.size = NSSize(width: widthPt, height: heightPt)

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    guard let cg = NSGraphicsContext.current?.cgContext else { fatalError("no CGContext") }
    cg.scaleBy(x: scale, y: scale)

    // Soft vertical wash, off-white into a faint cool gray.
    let top = CGColor(red: 0.976, green: 0.976, blue: 0.984, alpha: 1)
    let bottom = CGColor(red: 0.929, green: 0.933, blue: 0.945, alpha: 1)
    let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [top, bottom] as CFArray, locations: [0, 1]
    )!
    cg.drawLinearGradient(
        gradient,
        start: CGPoint(x: 0, y: heightPt), end: CGPoint(x: 0, y: 0),
        options: []
    )

    // Charcoal accent bar along the bottom edge — echoes the app's brand tile.
    cg.setFillColor(CGColor(red: 0.157, green: 0.165, blue: 0.184, alpha: 1))
    cg.fill(CGRect(x: 0, y: 0, width: widthPt, height: 6))

    // Dashed outline arrow pointing at the Applications alias. CoreGraphics
    // origin is bottom-left: icon centers sit 190pt from the TOP, i.e. y=210.
    let cx: CGFloat = 330
    let cy: CGFloat = heightPt - 190
    let shaftHalf: CGFloat = 16   // half-height of the shaft
    let headHalf: CGFloat = 34    // half-height of the arrowhead base
    let tail: CGFloat = cx - 46   // left edge of the shaft
    let neck: CGFloat = cx + 14   // where the shaft meets the head
    let tip: CGFloat = cx + 50    // arrow tip

    let arrow = CGMutablePath()
    arrow.move(to: CGPoint(x: tail, y: cy + shaftHalf))
    arrow.addLine(to: CGPoint(x: neck, y: cy + shaftHalf))
    arrow.addLine(to: CGPoint(x: neck, y: cy + headHalf))
    arrow.addLine(to: CGPoint(x: tip, y: cy))
    arrow.addLine(to: CGPoint(x: neck, y: cy - headHalf))
    arrow.addLine(to: CGPoint(x: neck, y: cy - shaftHalf))
    arrow.addLine(to: CGPoint(x: tail, y: cy - shaftHalf))
    arrow.closeSubpath()

    cg.addPath(arrow)
    cg.setStrokeColor(CGColor(red: 0.62, green: 0.64, blue: 0.68, alpha: 1))
    cg.setLineWidth(2.5)
    cg.setLineJoin(.round)
    cg.setLineCap(.round)
    cg.setLineDash(phase: 0, lengths: [7, 6])
    cg.strokePath()

    // Caption under the arrow, well clear of Finder's own icon labels.
    NSGraphicsContext.current?.cgContext.setLineDash(phase: 0, lengths: [])
    let caption = "Drag OxiMux to Applications" as NSString
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 13, weight: .medium),
        .foregroundColor: NSColor(calibratedRed: 0.45, green: 0.47, blue: 0.51, alpha: 1),
    ]
    let size = caption.size(withAttributes: attrs)
    caption.draw(
        at: NSPoint(x: cx - size.width / 2, y: 42),
        withAttributes: attrs
    )

    NSGraphicsContext.restoreGraphicsState()

    guard let png = rep.representation(using: .png, properties: [:]) else {
        fatalError("PNG encode failed")
    }
    try! png.write(to: url)
    print("wrote \(url.path) (\(pxW)x\(pxH))")
}

let fm = FileManager.default
let repoRoot = URL(fileURLWithPath: fm.currentDirectoryPath)
let tmp = repoRoot.appendingPathComponent("dist", isDirectory: true)
try? fm.createDirectory(at: tmp, withIntermediateDirectories: true)
let png1x = tmp.appendingPathComponent("dmg-bg-1x.png")
let png2x = tmp.appendingPathComponent("dmg-bg-2x.png")
render(scale: 1, to: png1x)
render(scale: 2, to: png2x)

// Fuse into a single HiDPI TIFF; Finder picks the right representation.
let out = repoRoot.appendingPathComponent("assets/dmg-background.tiff")
let task = Process()
task.executableURL = URL(fileURLWithPath: "/usr/bin/tiffutil")
task.arguments = ["-cathidpicheck", png1x.path, png2x.path, "-out", out.path]
try! task.run()
task.waitUntilExit()
guard task.terminationStatus == 0 else { fatalError("tiffutil failed") }
try? fm.removeItem(at: png1x)
try? fm.removeItem(at: png2x)
print("wrote \(out.path)")
