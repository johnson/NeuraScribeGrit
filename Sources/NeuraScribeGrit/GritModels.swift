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

    public init(path: String, status: String) {
        self.path = path
        self.status = status
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
