//  GritRepository.log — the commit walk the timeline depends on.
//
//  The point of these is the timestamp. Author time and committer time differ
//  only after a rebase or an amend, so a fixture built with real timestamps
//  cannot tell the two apart; these set both dates explicitly.

import Foundation
import Testing
@testable import NeuraScribeGrit

@Suite("Commit log walks by author date")
struct GritLogTests {

    /// A repo whose two commits have author dates in the opposite order to their
    /// committer dates — what rebasing leaves behind.
    private func makeRebasedFixture() throws -> URL {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("grit-log-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)

        func git(_ args: [String], author: String? = nil, committer: String? = nil) throws {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
            process.currentDirectoryURL = root
            process.arguments = args
            var environment = ProcessInfo.processInfo.environment
            if let author { environment["GIT_AUTHOR_DATE"] = author }
            if let committer { environment["GIT_COMMITTER_DATE"] = committer }
            process.environment = environment
            process.standardOutput = Pipe()
            process.standardError = Pipe()
            try process.run()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                throw NSError(domain: "fixture", code: Int(process.terminationStatus))
            }
        }

        try git(["init", "-q", "-b", "main"])
        try git(["config", "user.name", "Test"])
        try git(["config", "user.email", "test@example.com"])

        // Old work, rebased: author date stays in 2020, committer date is 2024.
        try "feature\n".write(to: root.appendingPathComponent("feature.md"), atomically: true, encoding: .utf8)
        try git(["add", "."])
        try git(["commit", "-q", "-m", "feature work"], author: "2020-01-02T03:04:05Z", committer: "2024-06-07T08:09:10Z")

        // Newer work whose committer date is older than the commit above.
        try "main\n".write(to: root.appendingPathComponent("main.md"), atomically: true, encoding: .utf8)
        try git(["add", "."])
        try git(["commit", "-q", "-m", "main work"], author: "2024-06-07T08:09:10Z", committer: "2020-01-02T03:04:05Z")

        return root
    }

    @Test("orders by author date, not committer date")
    func ordersByAuthorDate() throws {
        let root = try makeRebasedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let commits = try repo.log()

        #expect(commits.count == 2)
        // By committer date the first commit would lead; by author date it does not.
        #expect(commits[0].subject == "main work")
        #expect(commits[1].subject == "feature work")
        #expect(commits[0].date > commits[1].date)
    }

    @Test("carries the full message and a one-line subject")
    func carriesMessage() throws {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("grit-msg-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        for args in [["init", "-q", "-b", "main"], ["config", "user.name", "T"], ["config", "user.email", "t@e.com"]] {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
            process.currentDirectoryURL = root
            process.arguments = args
            try process.run()
            process.waitUntilExit()
        }
        try "x\n".write(to: root.appendingPathComponent("a.md"), atomically: true, encoding: .utf8)

        let commit = Process()
        commit.executableURL = URL(fileURLWithPath: "/usr/bin/git")
        commit.currentDirectoryURL = root
        commit.arguments = ["add", "."]
        try commit.run()
        commit.waitUntilExit()

        let write = Process()
        write.executableURL = URL(fileURLWithPath: "/usr/bin/git")
        write.currentDirectoryURL = root
        write.arguments = ["commit", "-q", "-m", "a subject that\nwraps onto a second line\n\nthe body, not the subject"]
        try write.run()
        write.waitUntilExit()

        let repo = try GritRepository(path: root.path)
        let first = try #require(try repo.log().first)

        #expect(first.subject == "a subject that wraps onto a second line")
        #expect(first.body.contains("the body, not the subject"))
    }

    @Test("since and until bound the range by author date")
    func filtersByAuthorDate() throws {
        let root = try makeRebasedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let commits = try repo.log()
        let oldest = try #require(commits.last?.date)
        let newest = try #require(commits.first?.date)

        // A bound below everything keeps both; above everything keeps none.
        #expect(try repo.log(since: oldest.addingTimeInterval(-1)).count == 2)
        #expect(try repo.log(until: newest.addingTimeInterval(1)).count == 2)
        #expect(try repo.log(since: newest).map(\.subject) == ["main work"])
        #expect(try repo.log(until: oldest).map(\.subject) == ["feature work"])

        // The window between the two commits keeps exactly the newer one.
        let between = oldest.addingTimeInterval((newest.timeIntervalSince(oldest)) / 2)
        #expect(try repo.log(since: between).map(\.subject) == ["main work"])
        #expect(try repo.log(until: between).map(\.subject) == ["feature work"])
        #expect(try repo.log(since: newest.addingTimeInterval(1)).isEmpty)
        #expect(try repo.log(until: oldest.addingTimeInterval(-1)).isEmpty)
    }

    @Test("limit applies after filtering, not before")
    func limitAppliesAfterFiltering() throws {
        let root = try makeRebasedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let repo = try GritRepository(path: root.path)

        let cutoff = try #require(try repo.log().last?.date)
        // Only one commit is in range; asking for one must still return that one
        // rather than nothing.
        #expect(try repo.log(limit: 1, since: cutoff).count == 1)
    }
}
