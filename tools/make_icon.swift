// Renders the ytui app icon (YouTube Music style: red circle, white play
// triangle) into an .iconset and packs it into an .icns.
//
//   swift tools/make_icon.swift assets/AppIcon.icns
//
// The generated .icns is committed, so this only needs to run when the design
// changes. It has no dependencies beyond the Xcode command line tools.
import AppKit
import CoreGraphics
import Foundation

let arguments = CommandLine.arguments
let output = arguments.count > 1 ? arguments[1] : "AppIcon.icns"
let iconset = FileManager.default.temporaryDirectory
    .appendingPathComponent("ytui-\(UUID().uuidString).iconset")
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
defer { try? FileManager.default.removeItem(at: iconset) }

func render(_ size: Int) -> Data {
    let scale = CGFloat(size) / 1024.0
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(
        data: nil,
        width: size,
        height: size,
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        fatalError("cannot create bitmap context")
    }
    ctx.setAllowsAntialiasing(true)
    ctx.setShouldAntialias(true)

    // YouTube red disc.
    let radius = 480.0 * scale
    ctx.setFillColor(CGColor(red: 1.0, green: 0.0, blue: 0.0, alpha: 1.0))
    ctx.fillEllipse(in: CGRect(
        x: 512.0 * scale - radius,
        y: 512.0 * scale - radius,
        width: radius * 2,
        height: radius * 2
    ))

    // White play triangle with rounded corners.
    let triangle = CGMutablePath()
    triangle.move(to: CGPoint(x: 408.0 * scale, y: 300.0 * scale))
    triangle.addLine(to: CGPoint(x: 408.0 * scale, y: 724.0 * scale))
    triangle.addLine(to: CGPoint(x: 764.0 * scale, y: 512.0 * scale))
    triangle.closeSubpath()
    ctx.addPath(triangle)
    ctx.setFillColor(CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 1.0))
    ctx.setStrokeColor(CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 1.0))
    ctx.setLineWidth(26.0 * scale)
    ctx.setLineJoin(.round)
    ctx.drawPath(using: .fillStroke)

    guard let image = ctx.makeImage() else { fatalError("cannot render icon") }
    let rep = NSBitmapImageRep(cgImage: image)
    guard let png = rep.representation(using: .png, properties: [:]) else {
        fatalError("cannot encode PNG")
    }
    return png
}

let variants: [(Int, String)] = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
]
for (size, name) in variants {
    try render(size).write(to: iconset.appendingPathComponent(name))
}

let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", output]
try iconutil.run()
iconutil.waitUntilExit()
if iconutil.terminationStatus != 0 {
    fatalError("iconutil failed")
}
print("wrote \(output)")
