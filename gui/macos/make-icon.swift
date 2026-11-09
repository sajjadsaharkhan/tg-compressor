// Draws the 1024x1024 app icon: a play button with compress arrows on a blue squircle.
// Usage: swift make-icon.swift out.png
import AppKit

let size = 1024.0
let out = CommandLine.arguments[1]

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// macOS icon grid: 824pt body inset 100pt, ~185pt corner radius, soft drop shadow.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let squircle = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: NSColor(white: 0, alpha: 0.3).cgColor)
ctx.addPath(squircle)
ctx.setFillColor(NSColor.black.cgColor)
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(squircle)
ctx.clip()
let gradient = CGGradient(
    colorsSpace: CGColorSpaceCreateDeviceRGB(),
    colors: [NSColor(red: 0.25, green: 0.78, blue: 0.98, alpha: 1).cgColor,
             NSColor(red: 0.16, green: 0.36, blue: 0.86, alpha: 1).cgColor] as CFArray,
    locations: [0, 1])!
ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: 924), end: CGPoint(x: 0, y: 100), options: [])
ctx.restoreGState()

let white = NSColor.white.cgColor
let center = CGPoint(x: 512, y: 512)

// Play triangle, optically centered (nudged right) with rounded corners.
let play = CGMutablePath()
play.move(to: CGPoint(x: center.x - 95, y: center.y + 140))
play.addLine(to: CGPoint(x: center.x + 150, y: center.y))
play.addLine(to: CGPoint(x: center.x - 95, y: center.y - 140))
play.closeSubpath()
ctx.addPath(play)
ctx.setLineJoin(.round)
ctx.setLineWidth(60)
ctx.setStrokeColor(white)
ctx.setFillColor(white)
ctx.drawPath(using: .fillStroke)

// Four corner arrows pointing inward: "make it smaller".
ctx.setLineWidth(38)
ctx.setLineCap(.round)
ctx.setStrokeColor(NSColor(white: 1, alpha: 0.85).cgColor)
for (dx, dy) in [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
    let tail = CGPoint(x: center.x + dx * 300, y: center.y + dy * 300)
    let tip = CGPoint(x: center.x + dx * 215, y: center.y + dy * 215)
    let head = 62.0
    ctx.move(to: tail)
    ctx.addLine(to: tip)
    ctx.move(to: CGPoint(x: tip.x + dx * head, y: tip.y))
    ctx.addLine(to: tip)
    ctx.addLine(to: CGPoint(x: tip.x, y: tip.y + dy * head))
    ctx.strokePath()
}

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
