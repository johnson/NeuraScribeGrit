//  Status latency, grit vs system git, on a real repository.
//
//  This is the number the whole plan rests on: NeuraScribe refreshes status on
//  every change, and upstream self-reports status at 3.1x slower than git.
//
//    swift run --scratch-path /tmp/nsg-swift-build gritBench <repo> [iterations]

import Foundation
import NeuraScribeGrit

@main
struct GritBench {
    static func main() {
        var args = Array(CommandLine.arguments.dropFirst())
        if args.first == "log" {
            args.removeFirst()
            logMode(args)
            return
        }
        if args.first == "diff" {
            args.removeFirst()
            diffMode(args)
            return
        }
        if args.first == "logsteps" {
            args.removeFirst()
            logSteps(args)
            return
        }
        guard let path = args.first else {
            print("usage: gritBench [log] <repo> [iterations]")
            return
        }
        let iterations = args.count > 1 ? (Int(args[args.index(args.startIndex, offsetBy: 1)]) ?? 0) : 0
        let n = iterations > 0 ? iterations : 20

        guard let repo = try? GritRepository(path: path) else {
            print("cannot open \(path)")
            return
        }

        // Warm the OS page cache so the first call is not an outlier.
        _ = try? repo.status()
        _ = gitStatus(path)

        let grit = measure(n) {
            _ = try repo.status()
        }
        let git = measure(n) {
            _ = gitStatus(path)
        }

        let files = (try? repo.status().files.count) ?? 0
        print("")
        print("repo: \(path)  tracked-ish files: \(files)  iterations: \(n)")
        print("  grit   \(grit.medianMs) ms  (min \(grit.minMs), max \(grit.maxMs))")
        print("  git    \(git.medianMs) ms  (min \(git.minMs), max \(git.maxMs))")
        if git.medianMs > 0 {
            print(String(format: "  ratio  %.2fx", grit.medianMs / git.medianMs))
        }
    }

    /// Latency of the Git History load path: opening the repository, then
    /// `log(limit:)` -- exactly what the app calls when the sheet appears.
    ///
    /// Both are reported cold and warm. The first call in a process is what the
    /// user actually waits for; the rest say what a repeat costs, and the gap
    /// between them is page cache, not work.
    static func logMode(_ args: [String]) {
        guard let path = args.first else {
            print("usage: gritBench log <repo> [iterations]")
            return
        }
        let n = args.count > 1 ? ((Int(args[1]) ?? 0) > 0 ? Int(args[1])! : 5) : 5

        var openMs: Double = 0
        var coldMs: Double = 0
        var repo: GritRepository?
        var walked = 0

        let openStart = DispatchTime.now().uptimeNanoseconds
        do {
            repo = try GritRepository(path: path)
            openMs = Double(DispatchTime.now().uptimeNanoseconds - openStart) / 1_000_000
        } catch {
            print("cannot open \(path): \(error)")
            return
        }
        guard let repo else { return }

        let coldStart = DispatchTime.now().uptimeNanoseconds
        let cold = try? repo.log(limit: 50)
        coldMs = Double(DispatchTime.now().uptimeNanoseconds - coldStart) / 1_000_000

        let warm = measure(n) {
            _ = try repo.log(limit: 50)
        }
        if let all = try? repo.log(limit: Int.max) {
            walked = all.count
        }

        let gitStart = DispatchTime.now().uptimeNanoseconds
        let gitOut = gitLog50(path)
        let gitMs = Double(DispatchTime.now().uptimeNanoseconds - gitStart) / 1_000_000

        print("")
        print("repo: \(path)  commits walked: \(walked)  iterations: \(n)")
        print(String(format: "  open            %8.1f ms", openMs))
        print(String(format: "  grit log(50)    %8.1f ms cold, %8.1f ms warm median (min %.1f, max %.1f)",
                     coldMs, warm.medianMs, warm.minMs, warm.maxMs))
        print(String(format: "  git  log -50    %8.1f ms (%d commits)", gitMs, gitOut))
        print("  grit returned \(cold?.count ?? -1) commit(s)")
    }

    /// What one whole-tree diff costs, which is what the daily review pays per
    /// commit and the home-screen feed used to pay for five it never read.
    static func diffMode(_ args: [String]) {
        guard let path = args.first else {
            print("usage: gritBench diff <repo> [commits]")
            return
        }
        let count = args.count > 1 ? (Int(args[1]) ?? 5) : 5
        guard let repo = try? GritRepository(path: path) else {
            print("cannot open \(path)")
            return
        }
        guard let commits = try? repo.log(limit: count) else {
            print("cannot read the log")
            return
        }

        var coldTotal = 0.0
        var totalFiles = 0
        for (index, commit) in commits.enumerated() {
            let start = DispatchTime.now().uptimeNanoseconds
            let files = (try? repo.diff(commit: commit.hash).count) ?? 0
            let ms = Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
            coldTotal += ms
            totalFiles += files
            print(String(format: "  diff #%d %@ %8.1f ms  %d file(s)", index, String(commit.hash.prefix(7)), ms, files))
        }

        let warm = measure(count) {
            _ = try? repo.diff(commit: commits[0].hash)
        }
        print("")
        print("repo: \(path)  commits=\(commits.count)  files changed across them=\(totalFiles)")
        print(String(format: "  first pass  %8.1f ms total", coldTotal))
        print(String(format: "  same diff   %8.1f ms warm median", warm.medianMs))
    }

    /// Where the first call's time goes, step by step, in one fresh process.
    ///
    /// A single "cold" number cannot be read: it mixes the one-time cost of
    /// opening the object store with the per-call cost of the walk, and only
    /// the second one is what a fix has to move.
    static func logSteps(_ args: [String]) {
        guard let path = args.first else {
            print("usage: gritBench logsteps <repo>")
            return
        }

        func ms(_ body: () throws -> Void) -> Double {
            let start = DispatchTime.now().uptimeNanoseconds
            try? body()
            return Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
        }

        var repo: GritRepository?
        let open = ms { repo = try? GritRepository(path: path) }
        guard let repo else { print("cannot open \(path)"); return }

        let head = ms { _ = try? repo.headSHA() }
        let one = ms { _ = try? repo.log(limit: 1) }
        let fifty = ms { _ = try? repo.log(limit: 50) }
        let window = ms {
            _ = try? repo.log(limit: 200, since: Date().addingTimeInterval(-7 * 86_400))
        }
        let again = ms { _ = try? repo.log(limit: 200, since: Date().addingTimeInterval(-7 * 86_400)) }
        let status = ms { _ = try? repo.status() }

        print("")
        print("repo: \(path)")
        print(String(format: "  open                 %8.1f ms", open))
        print(String(format: "  headSHA              %8.1f ms", head))
        print(String(format: "  log(1)     first     %8.1f ms   <- opens the object store", one))
        print(String(format: "  log(50)             %8.1f ms", fifty))
        print(String(format: "  log(200, 7d) first   %8.1f ms   <- what Git History asks for", window))
        print(String(format: "  log(200, 7d) again   %8.1f ms", again))
        print(String(format: "  status() first       %8.1f ms", status))
    }

    /// `git log --author-date-order -50` as the baseline: same ordering rule
    /// the shim uses, same 50 commits.
    static func gitLog50(_ path: String) -> Int {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
        process.arguments = ["log", "--author-date-order", "-50", "--format=%H"]
        process.currentDirectoryURL = URL(fileURLWithPath: path)
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = Pipe()
        try? process.run()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return String(decoding: data, as: UTF8.self)
            .split(separator: "\n")
            .count
    }

    struct Stats {
        var medianMs: Double
        var minMs: Double
        var maxMs: Double
    }

    static func measure(_ n: Int, _ body: () throws -> Void) -> Stats {
        var samples: [Double] = []
        samples.reserveCapacity(n)
        for _ in 0..<n {
            let start = DispatchTime.now().uptimeNanoseconds
            try? body()
            let end = DispatchTime.now().uptimeNanoseconds
            samples.append(Double(end - start) / 1_000_000)
        }
        samples.sort()
        return Stats(
            medianMs: samples[samples.count / 2],
            minMs: samples.first ?? 0,
            maxMs: samples.last ?? 0
        )
    }

    /// `git status --porcelain` as the baseline: same job, same repository.
    static func gitStatus(_ path: String) -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/git")
        process.arguments = ["status", "--porcelain"]
        process.currentDirectoryURL = URL(fileURLWithPath: path)
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = Pipe()
        try? process.run()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return String(decoding: data, as: UTF8.self)
    }
}
