// Rebuild the original Vardr icon with macOS system drawing tools.
import AppKit
import Foundation

let destination = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: destination, withIntermediateDirectories: true)
for size in [16, 32, 64, 128, 256, 512, 1024] {
    let image = NSImage(size: NSSize(width: size, height: size))
    image.lockFocus()
    let n = CGFloat(size)
    NSColor(red: 0.063, green: 0.082, blue: 0.09, alpha: 1).setFill()
    NSBezierPath(roundedRect: NSRect(x: n * 0.04, y: n * 0.04, width: n * 0.92, height: n * 0.92), xRadius: n * 0.2, yRadius: n * 0.2).fill()
    let diamond = NSBezierPath()
    diamond.move(to: NSPoint(x: n * 0.5, y: n * 0.8))
    diamond.line(to: NSPoint(x: n * 0.78, y: n * 0.5))
    diamond.line(to: NSPoint(x: n * 0.5, y: n * 0.2))
    diamond.line(to: NSPoint(x: n * 0.22, y: n * 0.5))
    diamond.close()
    NSColor(red: 0.737, green: 0.875, blue: 0.529, alpha: 1).setStroke()
    diamond.lineWidth = n * 0.065
    diamond.lineJoinStyle = .round
    diamond.stroke()
    NSColor(red: 0.737, green: 0.875, blue: 0.529, alpha: 1).setFill()
    NSBezierPath(ovalIn: NSRect(x: n * 0.45, y: n * 0.45, width: n * 0.1, height: n * 0.1)).fill()
    image.unlockFocus()
    let bitmap = NSBitmapImageRep(data: image.tiffRepresentation!)!
    let data = bitmap.representation(using: .png, properties: [:])!
    try data.write(to: destination.appendingPathComponent("\(size).png"))
}
