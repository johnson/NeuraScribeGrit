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
        var args = CommandLine.arguments.dropFirst()
        guard let path = args.first else {
            print("usage: gritBench <repo> [iterations]")
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
