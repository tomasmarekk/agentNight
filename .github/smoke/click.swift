// Clicks a point inside the main window of a process: swift click.swift <pid> <x> <y>
import CoreGraphics
import Foundation

let args = CommandLine.arguments
guard args.count == 4, let pid = Int32(args[1]), let dx = Double(args[2]), let dy = Double(args[3]) else {
    print("usage: click.swift <pid> <x> <y>")
    exit(2)
}

let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows {
    guard (window[kCGWindowOwnerPID as String] as? Int32) == pid,
          (window[kCGWindowLayer as String] as? Int) == 0,
          let dict = window[kCGWindowBounds as String],
          let bounds = CGRect(dictionaryRepresentation: dict as! CFDictionary)
    else { continue }
    let point = CGPoint(x: bounds.minX + dx, y: bounds.minY + dy)
    for type in [CGEventType.mouseMoved, .leftMouseDown, .leftMouseUp] {
        CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left)?
            .post(tap: .cghidEventTap)
        usleep(150_000)
    }
    print("clicked \(point) in window \(bounds)")
    exit(0)
}
print("no window found for pid \(pid)")
exit(1)
