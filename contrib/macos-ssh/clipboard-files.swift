import AppKit
import Foundation

// Only file URLs are read. Text, images and the general pasteboard are never
// written, and the result is consumed privately by the SSH companion.
func filePaths(_ board: NSPasteboard) -> [String] {
    let options: [NSPasteboard.ReadingOptionKey: Any] = [.urlReadingFileURLsOnly: true]
    return (board.readObjects(forClasses: [NSURL.self], options: options) as? [URL] ?? [])
        .filter { $0.isFileURL }.map { $0.path }
}

if CommandLine.arguments.dropFirst().first == "--self-test" {
    // An isolated pasteboard: tests never read or overwrite the user's clipboard.
    let board = NSPasteboard.withUniqueName()
    defer { board.releaseGlobally() }
    let paths = ["/tmp/space and 中文.png", "/tmp/quote'$(literal).txt"]
    board.writeObjects(paths.map { NSURL(fileURLWithPath: $0) })
    precondition(filePaths(board) == paths)
    board.clearContents()
    board.setString("/tmp/text-is-not-a-file", forType: .string)
    precondition(filePaths(board).isEmpty)
    board.clearContents()
    board.writeObjects([NSURL(string: "https://example.invalid/file")!])
    precondition(filePaths(board).isEmpty)
    print("Private pasteboard tests passed")
} else {
    let data = try JSONSerialization.data(withJSONObject: filePaths(.general))
    FileHandle.standardOutput.write(data)
}
