//  GritIndexTests - the `git rm -r --cached` primitives, on the Swift side.
//
//  The Rust suite checks the same behaviour against `git ls-files` and
//  `git rm --cached` directly. What is only visible here is the Swift layer:
//  the C-ABI signature, the JSON decode, and the `Int?` the guard branches on.

import Foundation
import Testing
@testable import NeuraScribeGrit

@Suite("Index operations from Swift")
struct GritIndexTests {

    /// A repo whose cache directory is tracked, with a tracked sibling and a
    /// file that exists on disk but was never added.
    private func makeFixture() throws -> URL {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("grit-index-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)

        func git(_ args: [String]) throws {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
            process.currentDirectoryURL = root
            process.arguments = args
            let output = Pipe()
            process.standardOutput = output
            process.standardError = Pipe()
            try process.run()
            let text = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                throw NSError(
                    domain: "fixture",
                    code: Int(process.terminationStatus),
                    userInfo: [NSLocalizedDescriptionKey: "git " + args.joined(separator: " ") + ": " + text]
                )
            }
        }

        func write(_ rel: String, _ body: String) throws {
            let url = root.appendingPathComponent(rel)
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try body.write(to: url, atomically: true, encoding: .utf8)
        }

        try git(["init", "-q", "-b", "main"])
        try git(["config", "user.name", "Test"])
        try git(["config", "user.email", "test@example.com"])
        try write("neurascribe.bundle/a.db", "a")
        try write("neurascribe.bundle/nested/b.db", "b")
        try write("notes.md", "n")
        try git(["add", "."])
        try git(["commit", "-q", "-m", "init"])
        // Written after the commit, so it is on disk but NOT in the index.
        try write("neurascribe-loose.txt", "untracked")

        return root
    }

    @Test("indexedPaths reports the nested files, sorted")
    func reportsTrackedPathsUnderADirectory() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let paths = try repo.indexedPaths(under: "neurascribe.bundle")
        #expect(paths == ["neurascribe.bundle/a.db", "neurascribe.bundle/nested/b.db"])
    }

    @Test("a file on disk that was never added is not reported")
    func ignoresUntrackedFiles() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        // `neurascribe-loose` has no index entries, only a file on disk.
        #expect(try repo.indexedPaths(under: "neurascribe-loose").isEmpty)
    }

    @Test("untrackDirectory drops the entries but keeps the files")
    func untrackKeepsTheWorkTree() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let removed = try repo.untrackDirectory("neurascribe.bundle")
        #expect(removed == 2)
        #expect(try repo.indexedPaths(under: "neurascribe.bundle").isEmpty)

        // Still there, untouched - this is `--cached`, not `rm`.
        #expect(FileManager.default.fileExists(atPath: root.appendingPathComponent("neurascribe.bundle/a.db").path))
        #expect(FileManager.default.fileExists(atPath: root.appendingPathComponent("neurascribe.bundle/nested/b.db").path))
        // And the unrelated file is still tracked.
        #expect(try repo.indexedPaths(under: "").contains("notes.md"))
    }

    @Test("untracking a directory nothing tracks reports 0, not an error")
    func untrackingNothingIsNotAnError() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        #expect(try repo.untrackDirectory("neurascribe-loose") == 0)
    }

    @Test("a sibling with a shared name prefix is left alone")
    func untrackRespectsComponentBoundaries() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        try try "x".write(to: root.appendingPathComponent("neurascribe-extra.txt"), atomically: true, encoding: .utf8)
        let repo = try GritRepository(path: root.path)
        try repo.stageAll()

        _ = try repo.untrackDirectory("neurascribe")

        // `neurascribe-extra.txt` is a sibling of `neurascribe.bundle`, not a child.
        #expect(try repo.indexedPaths(under: "").contains("neurascribe-extra.txt"))
    }
}