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
            throw GritError.failure()
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
        guard count >= 0 else { throw GritError.failure() }
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
        default: throw GritError.failure()
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
            //
            // Read right after the call, on this thread, and carried in the
            // error — the shim clears the slot on entry, so a message here is
            // this call's and not an earlier failure's.
            if let message = GritError.lastMessage() { throw GritError.lastFailed(message) }
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
    /// Paths and change kinds between two commits ("added", "modified", ...).
    ///
    /// Decodes `GritFileStatus`, not `GritFileDiff`: the shim reports the
    /// change kind for this operation and no patch, and a `GritFileDiff` decode
    /// fails on every call because its `patch` is non-optional.
    public func changedFileStatuses(from base: String, to head: String) throws -> [GritFileStatus] {
        try decode([GritFileStatus].self) {
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
        guard grit_repo_stage_all(handle) == 0 else { throw GritError.failure() }
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

    /// Commit only `files`, the way `git commit -- <paths>` does.
    ///
    /// Everything else the user had staged stays staged. A file-picker UI hands
    /// in a subset of the staged changes constantly, and committing the whole
    /// index instead would sweep in work the user did not tick.
    /// - Throws: `GritError` if none of `files` is actually staged.
    public func commit(
        message: String,
        files: [String],
        authorName: String? = nil,
        authorEmail: String? = nil
    ) throws -> String {
        let encoded = try JSONEncoder().encode(files)
        let json = String(decoding: encoded, as: UTF8.self)
        return try message.withCString { text in
            try (authorName ?? "NeuraScribe").withCString { name in
                try (authorEmail ?? "app@neurascribe.local").withCString { email in
                    try json.withCString { paths in
                        try takeString {
                            grit_repo_commit_only(handle, text, name, email, paths)
                        }
                    }
                }
            }
        }
    }

    /// Restore one file to its HEAD content, discarding local edits to it.
    public func restoreFile(_ path: String) throws {
        guard path.withCString({ grit_repo_reset_file_to_head(handle, $0) }) == 0 else {
            throw GritError.failure()
        }
    }

    /// Move HEAD to `sha` and make the working tree match it.
    @discardableResult
    public func resetHard(to sha: String) throws -> String {
        try takeString { sha.withCString { grit_repo_reset_hard_to(handle, $0) } }
    }

    /// Move HEAD to `sha` and rebuild the index from its tree, leaving the
    /// working tree exactly as it is.
    ///
    /// The mode recorded for each path comes from the tree, which is what makes
    /// this the repair for an index that picked up modes the trees never
    /// agreed with. Staged work is dropped, as `git reset --mixed` does;
    /// unstaged and untracked files are not touched.
    @discardableResult
    public func resetMixed(to sha: String) throws -> String {
        try takeString { sha.withCString { grit_repo_reset_mixed_to(handle, $0) } }
    }

    /// Whether the index already describes the tree HEAD commits to.
    ///
    /// Staging is not the same as changing anything: a dirty file can be staged
    /// and hash to the blob the tree already holds, and a submodule pointer can
    /// already sit at its recorded commit. A commit made then would carry its
    /// parent's tree, which records nothing and only adds noise to the history.
    /// Callers use this to decline to commit at all.
    ///
    /// - Throws: if the repository cannot be read, or HEAD has no tree.
    public func indexMatchesHead() throws -> Bool {
        switch grit_repo_index_matches_head(handle) {
        case 1:
            return true
        case 0:
            return false
        default:
            throw GritError.failure()
        }
    }

    /// Whether this repository can still read the object named by the SHA.
    ///
    /// The app's history lists are built from commit hashes and can outlive the
    /// objects behind them: a commit can be pruned, or made on another device
    /// and never fetched here. Every action on such a row then fails deep
    /// inside the object store with a bare hash, so callers check this first
    /// and can say something the user can act on.
    ///
    /// Absence is an ordinary answer, not an error, so a repository that does
    /// not have the object returns false without touching the last-error slot.
    ///
    /// - Throws: when the check itself could not be made -- a null handle, or a
    ///   string that is not an object id. Those used to be reported as `false`,
    ///   which is a different claim: "this commit is gone" instead of "I could
    ///   not find out". A caller acting on that difference either discards a
    ///   perfectly good commit or explains the wrong thing to the user.
    public func hasObject(_ sha: String) throws -> Bool {
        let result = sha.withCString { grit_repo_has_object(handle, $0) }
        if result < 0 { throw GritError.failure() }
        return result == 1
    }

    /// Re-checkout the current branch, discarding working-tree changes.
    @discardableResult
    public func forceCheckoutHEAD() throws -> String {
        try takeString { grit_repo_force_checkout_head(handle) }
    }

    /// Commits reachable from HEAD, newest first by **author** date.
    ///
    /// The window goes to grit with the request. It used to be applied here,
    /// after the shim had already walked the entire history and serialized every
    /// commit in it -- so asking for the last seven days of a repository with
    /// years of commits meant reading every one of those objects off disk to
    /// keep twenty-odd of them. The shim now stops as soon as the answer cannot
    /// grow, which is the difference between a timeline that opens and one that
    /// does not.
    ///
    /// `limit` is applied after filtering, so a caller asking for "20 commits
    /// since Tuesday" gets 20 commits from Tuesday, not the first 20 walked and
    /// then thinned out.
    ///
    /// - Parameters:
    ///   - limit: maximum number of commits to return.
    ///   - since: drop commits authored before this instant.
    ///   - until: drop commits authored after this instant.
    public func log(limit: Int = 20, since: Date? = nil, until: Date? = nil) throws -> [GritCommit] {
        let commits = try decode([GritCommit].self) {
            grit_repo_log_range_json(
                handle,
                max(limit, 1),
                Self.epochSeconds(since) ?? .min,
                Self.epochSeconds(until) ?? .max
            )
        }
        // The shim already applied the cap; this only guards the documented
        // contract against a limit it did not receive.
        return commits.prefix(max(limit, 1)).map { $0 }
    }

    /// Whole epoch seconds, which is the resolution the shim compares on.
    ///
    /// Fractional seconds are dropped rather than rounded: the shim filters on
    /// the integer second it parsed out of the commit, so rounding here would
    /// make a boundary instant mean something the walk cannot see.
    private static func epochSeconds(_ date: Date?) -> Int64? {
        guard let date else { return nil }
        return Int64(date.timeIntervalSince1970.rounded(.down))
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
            throw GritError.failure()
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

    // MARK: - Submodules

    /// Every submodule registered in `.gitmodules`, sorted by work-tree path.
    ///
    /// grit has no `submodule` porcelain, so this is assembled from the index
    /// gitlink plus `.gitmodules`. `indexedSha` is the commit the superproject
    /// records; `headSha` is where the work tree actually is — the two
    /// differing is what makes a submodule show up as modified.
    public func submodules() throws -> [GritSubmoduleInfo] {
        try decode([GritSubmoduleInfo].self) {
            grit_repo_submodules_json(handle)
        }
    }

    /// Register a submodule and stage its gitlink.
    ///
    /// The work tree must already be a clone of `url`; this writes the
    /// `.gitmodules` section and the gitlink at `sha`, and stages
    /// `.gitmodules` alongside it so one commit captures the whole thing.
    public func recordSubmodule(name: String, path: String, url: String, sha: String) throws {
        let ok = name.withCString { namePointer in
            path.withCString { pathPointer in
                url.withCString { urlPointer in
                    sha.withCString { shaPointer in
                        grit_repo_record_submodule(handle, namePointer, pathPointer, urlPointer, shaPointer)
                    }
                }
            }
        }
        guard ok == 0 else { throw GritError.failure() }
    }

    /// Remove a submodule: registration, gitlink, work tree and the
    /// submodule's own repository under `.git/modules`.
    public func removeSubmodule(path: String) throws {
        let ok = path.withCString { grit_repo_remove_submodule(handle, $0) }
        guard ok == 0 else { throw GritError.failure() }
    }

    /// Check a submodule out at the commit the superproject records.
    ///
    /// - Returns: the commit the submodule is now on.
    @discardableResult
    public func checkoutSubmodule(path: String, to sha: String) throws -> String {
        try decode(String.self) {
            path.withCString { pathPointer in
                sha.withCString { shaPointer in
                    grit_repo_checkout_submodule_to(handle, pathPointer, shaPointer)
                }
            }
        }
    }
    // MARK: - Index

    /// Paths git currently tracks below `directory`.
    ///
    /// - Not a directory listing: files present on disk that were never added
    ///   do not appear, which is what makes this a meaningful "is it still
    ///   tracked" check.
    public func indexedPaths(under directory: String) throws -> [String] {
        try decode([String].self) {
            directory.withCString {
                grit_repo_indexed_paths_under_json(handle, $0)
            }
        }
    }

    /// Stop tracking everything below `directory`, leaving the files on disk.
    ///
    /// This is `git rm -r --cached`.
    ///
    /// - Returns: how many index entries were removed; 0 when nothing there
    ///   was tracked.
    @discardableResult
    public func untrackDirectory(_ directory: String) throws -> Int {
        let ok = directory.withCString {
            grit_repo_untrack_directory(handle, $0)
        }
        guard ok >= 0 else { throw GritError.failure() }
        return Int(ok)
    }
    // MARK: - Branches

    /// Every local branch, sorted by name.
    public func branches() throws -> [GritBranchInfo] {
        try decode([GritBranchInfo].self) { grit_repo_list_branches_json(handle) }
    }

    /// Create `name` at `fromSHA` (HEAD when `nil`), returning its commit.
    ///
    /// Throws when the name is invalid or the branch already exists: this is
    /// the one operation where overwriting would be the wrong default.
    public func createBranch(named name: String, fromSHA: String? = nil) throws -> String {
        // Two C strings, two lifetimes. The call happens inside both
        // `withCString` closures; the string it returns is decoded afterwards,
        // because `takeString` throws and the produce closure must not.
        let start = fromSHA ?? ""
        var raw: UnsafeMutablePointer<CChar>?
        name.withCString { namePtr in
            start.withCString { startPtr in
                raw = grit_repo_create_branch(handle, namePtr, startPtr)
            }
        }
        return try takeString { raw }
    }

    /// Switch to `name` and update the work tree.
    ///
    /// Unlike `git checkout` this does **not** carry uncommitted changes
    /// across: grit has no three-way merge in the checkout path, so it would
    /// silently drop half of them. It throws instead, leaving the work tree
    /// and HEAD untouched.
    public func checkoutBranch(_ name: String) throws -> String {
        // Same shape as `createBranch`: the C string cannot outlive the
        // `withCString` body, and `takeString` throws, so decoding happens
        // after the pointer is handed back.
        var raw: UnsafeMutablePointer<CChar>?
        name.withCString { raw = grit_repo_checkout_branch_clean(handle, $0) }
        return try takeString { raw }
    }

    /// Delete a local branch.
    ///
    /// - Parameter force: pass `true` to allow discarding unmerged commits.
    ///   The current branch is refused either way.
    public func deleteBranch(_ name: String, force: Bool) throws {
        let result = name.withCString { grit_repo_delete_branch(handle, $0, force ? 1 : 0) }
        guard result == 0 else { throw GritError.failure() }
    }

    // MARK: - C plumbing

    /// The raw handle, for the network entry points in `GritNetwork`.
    ///
    /// Internal rather than public: callers hold a `GritRepository` and go
    /// through `GritNetwork`, not around it.
    var rawHandle: OpaquePointer { handle }

    /// Take ownership of a returned C string, freeing it either way.
    private func takeString(_ produce: () -> UnsafeMutablePointer<CChar>?) throws -> String {
        guard let pointer = produce() else { throw GritError.failure() }
        defer { grit_string_free(pointer) }
        return String(cString: pointer)
    }

    /// Decode a returned C string as JSON, freeing it either way.
    private func decode<T: Decodable>(
        _ type: T.Type,
        _ produce: () -> UnsafeMutablePointer<CChar>?
    ) throws -> T {
        guard let pointer = produce() else { throw GritError.failure() }
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
    /// The shim reported a failure. The message is whatever `grit_last_error`
    /// held *at the moment the call failed*, or nil when the shim recorded none.
    ///
    /// It is carried rather than looked up on demand on purpose. The shim keeps
    /// it in a thread-local, and every call the app makes runs in a detached
    /// task while the error is caught and rendered on the main actor — so
    /// reading the slot at render time reads a thread that never failed, and
    /// every grit error reached the user as a fixed no-message string.
    case lastFailed(String?)
    /// The shim returned something that is not the JSON the Swift side expects.
    case malformedResponse(String)

    public var errorDescription: String? {
        switch self {
        case .lastFailed(let message):
            message.map { detail in "grit: " + detail } ?? "grit reported a failure with no message"
        case .malformedResponse(let body):
            "grit returned something that is not the expected JSON: " + String(body.prefix(200))
        }
    }

    /// A failure, with the shim's own words for it.
    ///
    /// Call this *on the thread that made the failing call*. The thread-local it
    /// reads belongs to that thread and to no other.
    static func failure() -> GritError {
        .lastFailed(lastMessage())
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
