//  GritModels — the Swift counterparts of the serde types in `src/types.rs`.
//
//  Field names deliberately match the existing gix FFI structs
//  (`GixStatusResult` / `GixFileStatus` / `GixFileDiff` / `GixRemoteInfo`), so a
//  call site can move from gix to grit without touching anything but the name.

import Foundation

/// How one file differs from HEAD.
public struct GritFileStatus: Codable, Hashable, Sendable {
    /// Repository-relative path.
    public let path: String
    /// One of `modified`, `added`, `deleted`, `untracked`, `renamed`,
    /// `copied`, `type_changed`, `unmerged`.
    public let status: String
    /// The index differs from HEAD, so the next commit will contain this path.
    public let staged: Bool
    /// The working tree differs from the index, so the change is not staged.
    ///
    /// Both can be true at once — a staged file that was then edited again.
    public let unstaged: Bool

    public init(path: String, status: String, staged: Bool = false, unstaged: Bool = false) {
        self.path = path
        self.status = status
        self.staged = staged
        self.unstaged = unstaged
    }
}

/// Working-tree status against HEAD.
public struct GritStatus: Codable, Hashable, Sendable {
    /// Current branch name, or the raw SHA when HEAD is detached.
    public let branch: String
    /// Whether anything at all differs from HEAD.
    public let hasChanges: Bool
    /// Per-file entries, sorted by path.
    public let files: [GritFileStatus]

    enum CodingKeys: String, CodingKey {
        case branch
        case hasChanges = "has_changes"
        case files
    }
}

/// One file plus its unified-diff patch.
public struct GritFileDiff: Codable, Hashable, Sendable {
    public let path: String
    /// The unified-diff body. Empty for changes with no textual content, such
    /// as a pure permission change.
    public let patch: String

    public init(path: String, patch: String) {
        self.path = path
        self.patch = patch
    }
}

/// A configured remote.
public struct GritRemoteInfo: Codable, Hashable, Sendable {
    public let name: String
    public let url: String

    public init(name: String, url: String) {
        self.name = name
        self.url = url
    }
}

/// Credentials the app already holds for a remote.
///
/// grit would normally shell out to a `credential.helper`, which is not
/// available on iOS, so the app passes the password across instead. Field
/// names match `GixCredential` in the gix bindings.
public struct GritCredential: Codable, Hashable, Sendable {
    public let username: String
    public let password: String

    public init(username: String, password: String) {
        self.username = username
        self.password = password
    }

    /// Whether there is nothing worth sending.
    public var isEmpty: Bool {
        username.isEmpty && password.isEmpty
    }
    /// The JSON the shim expects, or nil when there is nothing to send.
    var shimJSON: String? {
        guard !isEmpty else { return nil }
        let encoder = JSONEncoder()
        return try? String(decoding: encoder.encode(self), as: UTF8.self)
    }
}

/// One commit, as the timeline needs it.
///
/// `date` is the **author** date, not the committer date. A rebase or an amend
/// moves the committer date and leaves the author's alone, so for a timeline the
/// committer date answers "when was this last touched on this machine" rather
/// than "when was this written". Both platform backends here use author time so
/// the two agree.
public struct GritCommit: Identifiable, Hashable, Sendable {
    public let hash: String
    /// Author time.
    public let date: Date
    /// First paragraph of the message, joined onto one line (git's `%s`).
    public let subject: String
    /// Full message, subject line included.
    public let body: String

    public var id: String { hash }
}

extension GritCommit: Decodable {
    private enum CodingKeys: String, CodingKey {
        case hash
        case authorTime = "author_time"
        case subject
        case message
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        hash = try container.decode(String.self, forKey: .hash)
        let seconds = try container.decode(TimeInterval.self, forKey: .authorTime)
        date = Date(timeIntervalSince1970: seconds)
        subject = try container.decode(String.self, forKey: .subject)
        body = try container.decode(String.self, forKey: .message)
    }
}

/// One submodule registration, with the state the app's UI shows.
public struct GritSubmoduleInfo: Codable, Hashable, Sendable {
    /// The `submodule.<name>` section name in `.gitmodules`.
    public let name: String
    /// Work-tree path relative to the repository root.
    public let path: String
    /// Clone URL exactly as written in `.gitmodules`.
    public let url: String
    /// The commit the superproject's index records; nil when there is no
    /// gitlink, which means the registration exists but was never staged.
    public let indexedSha: String?
    /// The commit the submodule's work tree is on; nil when not checked out.
    public let headSha: String?
    /// Whether the work tree exists with a usable git dir.
    public let initialized: Bool

    enum CodingKeys: String, CodingKey {
        case name, path, url, initialized
        case indexedSha = "indexed_sha"
        case headSha = "head_sha"
    }
}
/// One local branch.
public struct GritBranchInfo: Codable, Hashable, Sendable, Identifiable {
    public let name: String
    public let sha: String
    public let isHead: Bool
    public let upstream: String?
    public let ahead: Int
    public let behind: Int
    public let upstreamGone: Bool

    public var id: String { name }

    /// The shim serializes with serde's default snake_case; without this every
    /// decode throws and the caller silently sees an empty list.
    enum CodingKeys: String, CodingKey {
        case name, sha, upstream, ahead, behind
        case isHead = "is_head"
        case upstreamGone = "upstream_gone"
    }

    /// Short upstream name to show, or `nil` when there is nothing to show.
    ///
    /// `refs/remotes/origin/main` reads as `origin/main`; the full ref is an
    /// implementation detail of the ref store, not something to put in a menu.
    public var upstreamLabel: String? {
        guard let upstream else { return nil }
        if upstream.hasPrefix("refs/remotes/") {
            return String(upstream.dropFirst("refs/remotes/".count))
        }
        if upstream.hasPrefix("refs/heads/") {
            return String(upstream.dropFirst("refs/heads/".count))
        }
        return upstream
    }

    public init(
        name: String,
        sha: String,
        isHead: Bool,
        upstream: String?,
        ahead: Int,
        behind: Int,
        upstreamGone: Bool
    ) {
        self.name = name
        self.sha = sha
        self.isHead = isHead
        self.upstream = upstream
        self.ahead = ahead
        self.behind = behind
        self.upstreamGone = upstreamGone
    }
}


/// One entry of the stash stack.
public struct GritStash: Codable, Hashable, Sendable, Identifiable {
    /// Position in the stack, 0 being the most recent -- what `stash@{N}` means.
    public let index: Int
    public let sha: String
    /// The subject git would print for it, e.g. `WIP on main: 1a2b3c4 draft`.
    public let message: String
    /// Paths the stash would restore, tracked and untracked.
    public let paths: [String]

    public var id: Int { index }

    /// The selector that addresses this entry.
    public var selector: String { "stash@{\(index)}" }

    /// What to show in a menu.
    ///
    /// The subject git prints is already prefixed (`WIP on main:` /
    /// `On main:`), so repeating the branch in front of it would read as
    /// `main -- WIP on main: ...`.
    public var title: String { message.isEmpty ? "stash@{\(index)}" : message }
}

/// How a merge ended.
///
/// The four cases are not variations of one thing: one is nothing to do, one
/// is a ref move, one is a commit, and one is work the user still has to do.
/// Collapsing them into a Bool would make the conflict case indistinguishable
/// from success, which is the one case where the app must keep talking.
public enum GritMergeOutcome: Hashable, Sendable {
    /// The other side was already contained in HEAD.
    case upToDate
    /// HEAD moved to the other commit; no merge commit was created.
    case fastForward(head: String)
    /// A merge commit was created and the branch points at it.
    case merged(head: String)
    /// The merge is incomplete: HEAD has not moved and these paths need
    /// resolving before it can be finished.
    case conflicted(head: String, files: [String])

    /// Whether the user still has to resolve something.
    public var needsResolution: Bool {
        if case .conflicted = self { return true }
        return false
    }

    /// The commit HEAD now points at, when there is one.
    public var head: String? {
        switch self {
        case .upToDate: return nil
        case let .fastForward(head): return head
        case let .merged(head): return head
        case let .conflicted(head, _): return head
        }
    }

    /// The paths still carrying conflict markers.
    public var conflictedFiles: [String] {
        if case let .conflicted(_, files) = self { return files }
        return []
    }

    /// A sentence for the user, rather than a bare state name.
    public var summary: String {
        switch self {
        case .upToDate:
            return "Already up to date"
        case .fastForward:
            return "Fast-forwarded"
        case .merged:
            return "Merged"
        case let .conflicted(_, files):
            let count = files.count
            return count == 1
                ? "1 file has conflicts"
                : "\(count) files have conflicts"
        }
    }
}

/// The wire shape of `MergeOutcome` in `src/types.rs`.
struct GritMergePayload: Decodable {
    let kind: String
    let head: String?
    let files: [String]?

    enum CodingKeys: String, CodingKey {
        case kind
        case head
        case files
    }

    var outcome: GritMergeOutcome {
        switch kind {
        case "FastForward":
            return .fastForward(head: head ?? "")
        case "Merged":
            return .merged(head: head ?? "")
        case "Conflicted":
            return .conflicted(head: head ?? "", files: files ?? [])
        default:
            return .upToDate
        }
    }
}

/// Whether a merge is waiting to be finished, and what it is waiting on.
public struct GritMergeState: Codable, Hashable, Sendable {
    /// A merge was started and has not been finished or aborted.
    public let inProgress: Bool
    /// Paths with unmerged index entries.
    public let unresolved: [String]

    public init(inProgress: Bool, unresolved: [String]) {
        self.inProgress = inProgress
        self.unresolved = unresolved
    }

    /// Nothing left to resolve, so the merge can be committed.
    public var isReadyToFinish: Bool { inProgress && unresolved.isEmpty }

    enum CodingKeys: String, CodingKey {
        case inProgress = "in_progress"
        case unresolved
    }
}
