//  GritRepositoryTests — the Swift side, exercised against a real repository.
//
//  These build a throwaway repo with the system git and then read it back
//  through grit, so a passing test means the C ABI, the module map, the
//  xcframework and the Swift wrapper all agree.

import Foundation
import Testing
@testable import NeuraScribeGrit

@Suite("GritRepository against a real repository")
struct GritRepositoryTests {

    /// Build a small repo on disk: one commit, one staged change, one edit.
    private func makeFixture() throws -> URL {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("grit-swift-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)

        func git(_ args: [String], stdin: String? = nil) throws {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
            process.currentDirectoryURL = root
            process.arguments = args
            let output = Pipe()
            process.standardOutput = output
            process.standardError = Pipe()
            var input: Pipe?
            if stdin == nil {
                process.standardInput = FileHandle.nullDevice
            } else {
                let pipe = Pipe()
                input = pipe
                process.standardInput = pipe
            }
            try process.run()
            if let stdin {
                input?.fileHandleForWriting.write(Data(stdin.utf8))
                try input?.fileHandleForWriting.close()
            }
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
        try git(["init", "-q", "-b", "main"])
        try git(["config", "user.name", "Test"])
        try git(["config", "user.email", "test@example.com"])
        try try "hello\n".write(to: root.appendingPathComponent("a.md"), atomically: true, encoding: .utf8)
        try git(["add", "."])
        try git(["commit", "-q", "-m", "first"])

        return root
    }

    @Test("opens a repository and reports the branch and HEAD")
    func opensAndReadsBranch() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }

        #expect(GritRepository.exists(at: root.path))
        let repo = try GritRepository(path: root.path)
        #expect(try repo.currentBranch() == "main")
        #expect(try repo.headSHA().count == 40)
    }

    @Test("status is clean right after a commit, then dirty after an edit")
    func statusTracksTheWorkTree() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        #expect(try !repo.status().hasChanges)

        let extra = root.appendingPathComponent("b.md")
        try try "new\n".write(to: extra, atomically: true, encoding: .utf8)
        let dirty = try repo.status()
        #expect(dirty.hasChanges)
        #expect(dirty.files.contains { $0.path == "b.md" })
    }

    @Test("stage, commit, and read the commit back")
    func stageAndCommit() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        try try "changed\n".write(to: root.appendingPathComponent("a.md"), atomically: true, encoding: .utf8)
        try try "added\n".write(to: root.appendingPathComponent("c.md"), atomically: true, encoding: .utf8)
        try repo.stageAll()
        let sha = try repo.commit(message: "second", authorName: "Test", authorEmail: "test@example.com")

        #expect(sha.count == 40)
        #expect(try repo.headSHA() == sha)
        #expect(try !repo.status().hasChanges)
    }

    @Test("a commit the repository cannot read is reported as absent, not as an error")
    func hasObjectDistinguishesPresentFromMissing() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let head = try repo.headSHA()
        #expect(repo.hasObject(head))

        // Absence is an ordinary answer, so it must not throw and must not
        // consume the last-error slot the way a failure would.
        let absent = String(repeating: "0", count: 39) + "1"
        #expect(!repo.hasObject(absent))
        #expect(!repo.hasObject("not a sha"))
    }

    @Test("restoring a file discards the edit")
    func restoreFile() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let file = root.appendingPathComponent("a.md")
        try try "wrecked\n".write(to: file, atomically: true, encoding: .utf8)
        #expect(try repo.status().hasChanges)

        try repo.restoreFile("a.md")
        #expect(try !repo.status().hasChanges)
        #expect(try String(contentsOf: file, encoding: .utf8) == "hello\n")
    }

    @Test("ancestry and merge base agree with the shape of history")
    func ancestry() throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let first = try repo.headSHA()
        try try "two\n".write(to: root.appendingPathComponent("b.md"), atomically: true, encoding: .utf8)
        try repo.stageAll()
        let second = try repo.commit(message: "second")

        #expect(try repo.isAncestor(first, of: second))
        #expect(try !repo.isAncestor(second, of: first))
        #expect(try repo.mergeBase(of: first, and: second) == first)
    }

    @Test("opening a directory that is not a repository throws")
    func rejectsNonRepository() throws {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("grit-not-a-repo-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        #expect(throws: (any Error).self) {
            _ = try GritRepository(path: root.path)
        }
    }

    @Test("a failure keeps its message when it is read on the main actor")
    @MainActor
    func errorMessageSurvivesTheHopToTheUI() async throws {
        let root = try makeFixture()
        defer { try? FileManager.default.removeItem(at: root) }

        // Mirrors GitService.resetHardWithGrit and the catch that follows it in
        // the view: the grit call runs inside a detached task, and the error is
        // caught and rendered on another thread afterwards. The shim records the
        // reason in a thread-local, so a message read at render time looks at
        // the wrong slot.
        func failingReset(at path: String) async throws -> String {
            try await Task.detached(priority: .userInitiated) { [path] in
                let repo = try GritRepository(path: path)
                return try repo.resetHard(to: String(repeating: "a", count: 40))
            }.value
        }

        let description: String
        do {
            _ = try await failingReset(at: root.path)
            Issue.record("expected the reset to fail")
            description = ""
        } catch {
            description = error.localizedDescription
        }

        // The point is not the exact wording, it is that grit's words arrive
        // at all. When the message was read lazily at render time, every grit
        // failure reached the user as a fixed no-message string.
        #expect(!description.isEmpty)
        #expect(!description.contains("no message"),
            "the message was lost across the thread hop: \(description)")
    }
}
