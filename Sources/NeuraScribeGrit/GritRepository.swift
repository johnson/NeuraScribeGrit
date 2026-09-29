//  GritRepository — the Swift face of the local operations the shim exports.
//
//  Scope: everything that touches only the working tree. Clone, fetch and push
//  live in GritNetwork and are not wired up yet, so this type deliberately
//  makes no network calls.

import Foundation

#if canImport(neurascribe_gritFFI)
import neurascribe_gritFFI
#endif

/// A repository opened through grit.
///
/// Hold one per open project and drop it when the project closes. Opening is
/// cheap — grit reads refs and config lazily — and every operation re-reads
/// state from disk, so a long-lived handle behaves like an open file.
public final class GritRepository {
    private let handle: OpaquePointer

    /// Opens the repository rooted at `path`, or throws.
    ///
    /// - Parameter path: the `.git` directory or the working tree root.
    public init(path: String) throws {
        guard let handle = path.withCString({ grit_repo_open($0) }) else {
            throw GritError.lastFailed
        }
        self.handle = handle
    }

    deinit {
        grit_repo_free(handle)
    }

    /// Whether `path` looks like a repository root.
    public static func exists(at path: String) -> Bool {
        path.withCString { grit_repo_exists($0) != 0 }
    }

    // MARK: - Reading

    /// Working-tree status against HEAD.
    public func status() throws -> GritStatus {
        try decode(GritStatus.self) { grit_repo_status_json(handle) }
    }

    /// The checked-out branch name.
    public func currentBranch() throws -> String {
        try takeString { grit_repo_current_branch(handle) }
    }

    /// The commit HEAD points at.
    public func headSHA() throws -> String {
        try takeString { grit_repo_head_sha(handle) }
    }

    /// How far `remoteTip` is ahead of the local branch.
    public func aheadCount(comparedTo remoteTip: String) throws -> Int {
        let count = remoteTip.withCString { grit_repo_ahead_count(handle, $0) }
        guard count >= 0 else { throw GritError.lastFailed }
        return Int(count)
    }

    /// Whether `a` is an ancestor of `b`.
    public func isAncestor(_ a: String, of b: String) throws -> Bool {
        let result = a.withCString { lhs in
            b.withCString { rhs in grit_repo_is_ancestor(handle, lhs, rhs) }
        }
        switch result {
        case 1: return true
        case 0: return false
        default: throw GritError.lastFailed
        }
    }

    /// Best common ancestor of two commits, or nil when they are unrelated.
    public func mergeBase(of a: String, and b: String) throws -> String? {
        let raw = a.withCString { lhs in
            b.withCString { rhs in grit_repo_merge_base(handle, lhs, rhs) }
        }
        guard let raw else {
            // NULL is ambiguous here: unrelated commits and a real failure look
            // identical, and only the thread-local error slot separates them.
            if let message = GritError.lastMessage() { throw GritError.lastFailed }
            return nil
        }
        defer { grit_string_free(raw) }
        return String(cString: raw)
    }

    /// Unified diff of one path, HEAD against the working tree.
    public func diff(file path: String) throws -> String {
        try takeString { path.withCString { grit_repo_diff_file(handle, $0) } }
    }

    /// Per-file change list for one commit.
    public func diff(commit sha: String) throws -> [GritFileDiff] {
        try decode([GritFileDiff].self) {
            sha.withCString { grit_repo_diff_commit_json(handle, $0) }
        }
    }

    /// Per-file change list between two commits.
    public func changedFiles(from base: String, to head: String) throws -> [GritFileDiff] {
        try decode([GritFileDiff].self) {
            base.withCString { lhs in head.withCString { rhs in grit_repo_changed_files_json(handle, lhs, rhs) } }
        }
    }

    /// The text of one file as of `sha`, or nil when it does not exist there.
    public func text(of path: String, at sha: String) throws -> String? {
        let raw = sha.withCString { commit in
            path.withCString { file in grit_repo_blob_text(handle, commit, file) }
        }
        guard let raw else { return nil }
        defer { grit_string_free(raw) }
        return String(cString: raw)
    }

    /// Absolute path of the working tree, for handing to file pickers.
    public func workTreePath() throws -> String {
        try takeString { grit_repo_work_tree(handle) }
    }

    /// Configured remotes. The shim returns a list, not a map.
    public func remotes() throws -> [GritRemoteInfo] {
        try decode([GritRemoteInfo].self) { grit_repo_list_remotes_json(handle) }
    }

    /// The first configured remote, or nil when there are none.
    ///
    /// NeuraScribe assumes a single remote in several places, so this
    /// keeps those call sites from each re-implementing the same preference.
    public func primaryRemote() throws -> GritRemoteInfo? {
        let all = try remotes()
        return all.first { $0.name == "origin" } ?? all.first
    }

    // MARK: - Writing

    /// Stage every change, deletions included.
    public func stageAll() throws {
        // 0 is success here and -1 is failure; the inverse reads as a bug.
        guard grit_repo_stage_all(handle) == 0 else { throw GritError.lastFailed }
    }

    /// Commit the staged changes and return the new commit id.
    ///
    /// - Parameters:
    ///   - message: the commit message.
    ///   - authorName: falls back to `NeuraScribe` when nil.
    ///   - authorEmail: falls back to `app@neurascribe.local` when nil.
    @discardableResult
    public func commit(message: String, authorName: String? = nil, authorEmail: String? = nil) throws -> String {
        try message.withCString { text in
            try (authorName ?? "NeuraScribe").withCString { name in
                try (authorEmail ?? "app@neurascribe.local").withCString { email in
                    try takeString { grit_repo_commit(handle, text, name, email) }
                }
            }
        }
    }

    /// Restore one file to its HEAD content, discarding local edits to it.
    public func restoreFile(_ path: String) throws {
        guard path.withCString({ grit_repo_reset_file_to_head(handle, $0) }) == 0 else {
            throw GritError.lastFailed
        }
    }

    /// Move HEAD to `sha` and make the working tree match it.
    @discardableResult
    public func resetHard(to sha: String) throws -> String {
        try takeString { sha.withCString { grit_repo_reset_hard_to(handle, $0) } }
    }

    /// Re-checkout the current branch, discarding working-tree changes.
    @discardableResult
    public func forceCheckoutHEAD() throws -> String {
        try takeString { grit_repo_force_checkout_head(handle) }
    }

    /// Commits reachable from HEAD, newest first by **author** date.
    ///
    /// The range is filtered here rather than in grit, because `since` / `until`
    /// are author-time bounds and the shim only walks. `limit` is applied after
    /// filtering, so a caller asking for "20 commits since Tuesday" gets 20
    /// commits from Tuesday, not the first 20 walked and then thinned out.
    ///
    /// - Parameters:
    ///   - limit: maximum number of commits to return.
    ///   - since: drop commits authored before this instant.
    ///   - until: drop commits authored after this instant.
    public func log(limit: Int = 20, since: Date? = nil, until: Date? = nil) throws -> [GritCommit] {
        // Ask for everything the walk finds, then filter, then cut. A capped
        // walk would make `since` silently wrong whenever more than `limit`
        // commits fall outside the window.
        let walked = try decode([GritCommit].self) {
            grit_repo_log_json(handle, 0)
        }
        return walked
            .filter { commit in
                if let since, commit.date < since { return false }
                if let until, commit.date > until { return false }
                return true
            }
            .prefix(max(limit, 1))
            .map { $0 }
    }

    /// Resolve a ref name (or a raw SHA) to a commit SHA.
    ///
    /// Reads `packed-refs` as well as loose refs, so a ref that git packed
    /// after a fetch still resolves.
    public func resolveRef(_ name: String) throws -> String {
        try decode(String.self) {
            grit_repo_resolve_ref(handle, name)
        }
    }

    /// Every tracked path at a commit, repo-relative and sorted.
    ///
    /// Recursive, so nested paths come back as full repo-relative paths.
    public func listFiles(at sha: String) throws -> [String] {
        try decode([String].self) {
            grit_repo_list_files_json(handle, sha)
        }
    }

    /// Stage a single path, recording a deletion when the file is gone.
    public func stageFile(_ path: String) throws {
        guard grit_repo_stage_file(handle, path) == 0 else {
            throw GritError.lastFailed
        }
    }

    /// Fast-forward HEAD and the work tree to a commit.
    ///
    /// - Throws: when the histories have diverged. This never merges; the pull
    ///   coordinator decides up front whether a merge is needed, and merging
    ///   here would take that decision away from it.
    /// - Returns: the new HEAD SHA.
    @discardableResult
    public func fastForward(to tip: String) throws -> String {
        try decode(String.self) {
            grit_repo_fast_forward_json(handle, tip)
        }
    }

    // MARK: - C plumbing

    /// The raw handle, for the network entry points in `GritNetwork`.
    ///
    /// Internal rather than public: callers hold a `GritRepository` and go
    /// through `GritNetwork`, not around it.
    var rawHandle: OpaquePointer { handle }

    /// Take ownership of a returned C string, freeing it either way.
    private func takeString(_ produce: () -> UnsafeMutablePointer<CChar>?) throws -> String {
        guard let pointer = produce() else { throw GritError.lastFailed }
        defer { grit_string_free(pointer) }
        return String(cString: pointer)
    }

    /// Decode a returned C string as JSON, freeing it either way.
    private func decode<T: Decodable>(
        _ type: T.Type,
        _ produce: () -> UnsafeMutablePointer<CChar>?
    ) throws -> T {
        guard let pointer = produce() else { throw GritError.lastFailed }
        defer { grit_string_free(pointer) }
        do {
            return try JSONDecoder().decode(T.self, from: Data(String(cString: pointer).utf8))
        } catch {
            throw GritError.malformedResponse(String(cString: pointer))
        }
    }
}

/// A failure from the grit backend.
public enum GritError: LocalizedError {
    /// The shim reported a failure; the detail came from `grit_last_error`.
    case lastFailed
    /// The shim returned something that is not the JSON the Swift side expects.
    case malformedResponse(String)

    public var errorDescription: String? {
        switch self {
        case .lastFailed:
            GritError.lastMessage().map { detail in "grit: " + detail } ?? "grit reported a failure with no message"
        case .malformedResponse(let body):
            "grit returned something that is not the expected JSON: " + String(body.prefix(200))
        }
    }

    /// The error recorded on the calling thread, or nil when there is none.
    ///
    /// Thread-local in the shim, so read it on the thread that made the failing
    /// call, before any other grit call overwrites it.
    static func lastMessage() -> String? {
        guard let raw = grit_last_error() else { return nil }
        defer { grit_string_free(raw) }
        let message = String(cString: raw)
        return message.isEmpty ? nil : message
    }
}
