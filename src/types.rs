//! Serde types mirroring the data shapes the existing gix FFI (`GitRepo`) returns,
//! so the Swift call sites can be swapped with a type-only change.
//!
//! Field names intentionally match `GixStatusResult` / `GixFileStatus` / `GixFileDiff`
//! / `GixRemoteInfo` in `neurascribe_git_ffi.swift`.

use serde::Serialize;

/// One file entry in a status or changed-files result.
#[derive(Debug, Clone, Serialize)]
pub struct FileStatus {
    /// Repository-relative path.
    pub path: String,
    /// One of: `modified`, `added`, `deleted`, `untracked`, `renamed`, `copied`,
    /// `type_changed`, `unmerged`.
    pub status: String,
    /// The index differs from HEAD, so the next commit will contain this path.
    ///
    /// git shows this and `unstaged` as two columns. Collapsing them into one
    /// row loses the distinction the UI needs: a file that was staged and then
    /// edited again has to appear as both staged and modified.
    pub staged: bool,
    /// The working tree differs from the index, so the change is not staged yet.
    pub unstaged: bool,
}

/// Full working-tree status.
#[derive(Debug, Clone, Serialize)]
pub struct StatusResult {
    /// Current branch name, or the raw SHA when HEAD is detached.
    pub branch: String,
    /// Whether any tracked or untracked change exists.
    pub has_changes: bool,
    /// Per-file entries, sorted by path.
    pub files: Vec<FileStatus>,
}

/// One file plus its unified-diff patch.
#[derive(Debug, Clone, Serialize)]
pub struct FileDiff {
    pub path: String,
    /// Unified diff body. Empty string when the change has no textual content
    /// (e.g. a pure mode change).
    pub patch: String,
}

/// A configured remote.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
}

/// One local branch.
#[derive(Debug, Clone, Serialize)]
pub struct BranchInfo {
    /// Short name, without the `refs/heads/` prefix.
    pub name: String,
    /// Commit the branch points at.
    pub sha: String,
    /// True when HEAD currently points at this branch.
    pub is_head: bool,
    /// Full upstream ref (`refs/remotes/...` or `refs/heads/...`), when the
    /// branch has tracking configuration.
    pub upstream: Option<String>,
    /// Commits on the branch but not on its upstream.
    pub ahead: u32,
    /// Commits on the upstream but not on the branch.
    pub behind: u32,
    /// Tracking is configured but the upstream ref does not resolve locally
    /// (never fetched, or the remote was deleted). Kept separate from
    /// `upstream == nil`, which means "no tracking configured at all".
    pub upstream_gone: bool,
}

/// Credentials the app already holds for a remote.
///
/// grit would normally consult a `credential.helper`, but that is a subprocess
/// and unavailable on iOS, so the app hands the password over directly. Field
/// names match `GixCredential` in `neurascribe_git_ffi.swift`.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

impl Credential {
    /// True when neither half carries anything worth sending.
    pub fn is_empty(&self) -> bool {
        self.username.is_empty() && self.password.is_empty()
    }
}

/// How a merge finished.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum MergeOutcome {
    /// Fast-forward; HEAD now points at the other commit.
    FastForward { head: String },
    /// A merge commit was created.
    Merged { head: String },
    /// Merged but the tree has conflicts; the caller must resolve.
    Conflicted { head: String, files: Vec<String> },
    /// Nothing to do — already up to date.
    UpToDate,
}
