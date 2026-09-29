//  Diagnostic harness: run each operation one at a time and print what grit
//  actually said. Not part of the test suite.
//
//    swift run --scratch-path /tmp/nsg-swift-build gritDiag <path-to-repo>

import Foundation
import NeuraScribeGrit

@main
struct GritDiag {
    static func main() {
        guard CommandLine.arguments.count > 1 else {
            print("usage: gritDiag <repo>")
            return
        }
        let path = CommandLine.arguments[1]
        do {
            let repo = try GritRepository(path: path)
            report("currentBranch") { try repo.currentBranch() }
            report("headSHA") { try repo.headSHA() }
            report("status") { try repo.status() }
            report("stageAll") { try repo.stageAll(); return "ok" }
            report("restoreFile(a.md)") { try repo.restoreFile("a.md"); return "ok" }
            report("workTreePath") { try repo.workTreePath() }
            report("remotes") { try repo.remotes().description }
            report("diffFile(a.md)") { try repo.diff(file: "a.md") }
            if let head = try? repo.headSHA() {
                report("mergeBase(head,head)") { try repo.mergeBase(of: head, and: head) ?? "nil" }
                report("isAncestor(head,head)") { try repo.isAncestor(head, of: head).description }
                report("changedFileStatuses(head,head)") { try repo.changedFileStatuses(from: head, to: head).description }
                report("blobText(a.md)") { try repo.text(of: "a.md", at: head) ?? "nil" }
                report("diffCommit(head)") { try repo.diff(commit: head).description }
            }
        } catch {
            print("open FAILED: \(error.localizedDescription)")
        }
    }

    static func report<T>(_ label: String, _ body: () throws -> T) {
        do {
            let value = try body()
            print("\(label): ok -> \(value)")
        } catch {
            print("\(label): FAILED -> \(error.localizedDescription)")
        }
    }
}
