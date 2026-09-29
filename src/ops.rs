//! Git operations implemented directly on `grit-lib` primitives.
//!
//! grit-lib is an engine, not a porcelain: `lib.rs` ships an empty `plumbing`
//! module and a `porcelain` module that contains exactly one finished
//! operation (`status`). Everything else here is assembled from building blocks,
//! following the recipes in the MIT-licensed `grit-cli` crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::diff::{diff_index_to_tree, diff_index_to_worktree, diff_trees, unified_diff_histogram_with_prefix_and_funcname, DiffStatus};
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_REGULAR};
use grit_lib::merge_base::{count_symmetric_ahead_behind, is_ancestor as grit_is_ancestor, merge_bases_octopus};
use grit_lib::objects::{parse_commit, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, OrderingMode, RevListOptions};
use grit_lib::write_tree::{write_tree_from_index, write_tree_partial_from_index};

use crate::types::{FileDiff, FileStatus, RemoteInfo, StatusResult};

/// Open a repository by walking up from `path` (the same discovery rule git uses).
pub fn open(path: &Path) -> Result<Repository> {
    Repository::discover(Some(path)).with_context(|| format!("not a git repository: {}", path.display()))
}

/// True when `path` is inside a work tree that has a `.git`.
pub fn exists(path: &Path) -> bool {
    Repository::discover(Some(path)).is_ok()
}

/// Current branch name; falls back to the raw HEAD sha when detached.
pub fn current_branch(repo: &Repository) -> Result<String> {
    if let Some(target) = grit_lib::refs::read_head(&repo.git_dir)? {
        return Ok(target.strip_prefix("refs/heads/").unwrap_or(&target).to_string());
    }
    head_sha(repo).ok_or_else(|| anyhow!("HEAD is neither a symbolic ref nor a commit"))
}

/// Resolve HEAD to a commit sha, or `None` for an unborn branch.
pub fn head_sha(repo: &Repository) -> Option<String> {
    grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD").ok().map(|o| o.to_string())
}

/// Working-tree status. Delegates to the one porcelain operation grit-lib does
/// provide, which gives real `.gitignore` handling, untracked collection and
/// optional rename detection instead of a hand-rolled approximation.
pub fn status(repo: &Repository) -> Result<StatusResult> {
    let opts = grit_lib::porcelain::status::StatusOptions {
        // `All` lists individual files inside untracked directories, which is
        // what the app shows; `Normal` would report the directory itself.
        untracked: grit_lib::porcelain::status::UntrackedMode::All,
        ignored: grit_lib::porcelain::status::IgnoredMode::No,
        renames: None,
        pathspecs: Vec::new(),
        // Ahead/behind is computed by a separate call path; skip it here.
        ahead_behind: false,
    };
    let model = grit_lib::porcelain::status::status(repo, &opts, &mut NullProgress)?;

    // Merge by path rather than concatenating and deduplicating. A path can be
    // staged and then edited again in the working tree; git shows that as two
    // columns and the app needs both halves, so the entry is one row carrying
    // two flags. A BTreeMap also gives the sorted-by-path order for free.
    let mut by_path: BTreeMap<String, FileStatus> = BTreeMap::new();

    for entry in &model.staged {
        by_path
            .entry(entry.path().to_string())
            .or_insert_with(|| FileStatus {
                path: entry.path().to_string(),
                status: status_name(&entry.status).to_string(),
                staged: false,
                unstaged: false,
            })
            .staged = true;
    }

    for entry in &model.unstaged {
        let path = entry.path().to_string();
        let slot = by_path
            .entry(path.clone())
            .or_insert_with(|| FileStatus {
                path,
                status: status_name(&entry.status).to_string(),
                staged: false,
                unstaged: false,
            });
        slot.unstaged = true;
        // Only take the kind from the work-tree side when nothing is staged:
        // for a staged path, the staged kind is the one the next commit gets.
        if !slot.staged {
            slot.status = status_name(&entry.status).to_string();
        }
    }

    for path in &model.untracked {
        by_path
            .entry(path.clone())
            .or_insert_with(|| FileStatus {
                path: path.clone(),
                status: "untracked".to_string(),
                staged: false,
                unstaged: false,
            });
    }

    let files: Vec<FileStatus> = by_path.into_values().collect();

    Ok(StatusResult {
        branch: current_branch(repo).unwrap_or_default(),
        has_changes: !files.is_empty(),
        files,
    })
}

/// Stage every change reported by status and write the index.
pub fn stage_all(repo: &Repository) -> Result<()> {
    let work_tree = repo.work_tree.as_deref().context("bare repository has no work tree")?;
    let mut index = repo.load_index()?;

    // Drive the real status walk so ignore rules match `git add -A`.
    let opts = grit_lib::porcelain::status::StatusOptions {
        untracked: grit_lib::porcelain::status::UntrackedMode::All,
        ignored: grit_lib::porcelain::status::IgnoredMode::No,
        renames: None,
        pathspecs: Vec::new(),
        ahead_behind: false,
    };
    let model = grit_lib::porcelain::status::status(repo, &opts, &mut NullProgress)?;

    // Additions and modifications: any unstaged or untracked path.
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for entry in &model.unstaged {
        touched.insert(entry.path().to_string());
    }
    for path in &model.untracked {
        touched.insert(path.clone());
    }
    for rel in touched {
        let full = work_tree.join(&rel);
        if full.is_file() {
            add_path(repo, &mut index, &full)?;
        }
    }

    // Deletions: tracked paths that no longer exist on disk.
    let tracked: Vec<String> = index
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).to_string())
        .collect();
    for rel in tracked {
        if !work_tree.join(&rel).exists() {
            index.entries.retain(|e| String::from_utf8_lossy(&e.path) != rel);
        }
    }

    index.entries.sort_by(|a, b| a.path.cmp(&b.path));
    write_index_clean(repo, &mut index)?;
    Ok(())
}

/// Commit the current index. Returns the new commit sha.
pub fn commit(repo: &Repository, message: &str, author_name: &str, author_email: &str) -> Result<String> {
    let index = repo.load_index()?;
    if index.entries.is_empty() {
        bail!("nothing to commit (index is empty)");
    }

    let tree = write_tree_from_index(&repo.odb, &index, "")?;
    let mut parents: Vec<ObjectId> = Vec::new();
    if let Some(sha) = head_sha(repo) {
        parents.push(sha.parse().context("parse HEAD sha")?);
    }

    let ident = format_ident(author_name, author_email);
    let commit = CommitData {
        tree,
        parents,
        author: ident.clone(),
        committer: ident,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{}\n", message.trim_end()),
        raw_message: None,
    };
    let oid = repo.odb.write(ObjectKind::Commit, &serialize_commit(&commit))?;

    // Move the ref HEAD points at; a detached HEAD updates HEAD itself.
    let target =
        grit_lib::refs::read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_string());
    grit_lib::refs::write_ref(&repo.git_dir, &target, &oid)?;
    append_reflog(repo, &target, &oid);
    Ok(oid.to_string())
}

/// Commit only `paths`, the way `git commit -- <paths>` does.
///
/// Everything else the user had staged stays staged. The commit's tree is
/// therefore HEAD with just those paths overwritten, not the index filtered --
/// a staged deletion the user did not name has to stay deleted, and a staged
/// edit they did not name has to stay out.
pub fn commit_only(
    repo: &Repository,
    message: &str,
    author_name: &str,
    author_email: &str,
    paths: &[String],
) -> Result<String> {
    let index = repo.load_index()?;
    let wanted: std::collections::HashSet<Vec<u8>> =
        paths.iter().map(|p| p.as_bytes().to_vec()).collect();

    // An unborn HEAD commits against the empty tree, so there is nothing to
    // carry forward and the result is just the selected paths.
    let head = head_sha(repo);
    let base_tree_oid = match &head {
        Some(sha) => commit_tree(repo, sha)?,
        None => repo.odb.write(ObjectKind::Tree, &[])?,
    };

    // "Staged" has to mean the index entry differs from HEAD, not merely that the
    // path is in the index. Otherwise committing a path the user edited but
    // never staged would quietly create an empty commit.
    let head_tree: BTreeMap<String, ObjectId> = match &head {
        Some(sha) => tree_map(repo, sha)?,
        None => BTreeMap::new(),
    };
    let staged_any = index.entries.iter().any(|e| {
        wanted.contains(&e.path)
            && e.stage() == 0
            && head_tree
            .get(String::from_utf8_lossy(&e.path).as_ref())
            .copied()
            != Some(e.oid)
    });
    if !staged_any {
        bail!("none of the selected paths are staged");
    }

    let tree = write_tree_partial_from_index(&repo.odb, &index, &base_tree_oid, &wanted)?;

    let mut parents: Vec<ObjectId> = Vec::new();
    if let Some(sha) = &head {
        parents.push(sha.parse().context("parse HEAD sha")?);
    }

    let ident = format_ident(author_name, author_email);
    let commit = CommitData {
        tree,
        parents,
        author: ident.clone(),
        committer: ident,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{}\n", message.trim_end()),
        raw_message: None,
    };
    let oid = repo.odb.write(ObjectKind::Commit, &serialize_commit(&commit))?;

    let target =
        grit_lib::refs::read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_string());
    grit_lib::refs::write_ref(&repo.git_dir, &target, &oid)?;
    append_reflog(repo, &target, &oid);
    Ok(oid.to_string())
}

/// Unified diff of one file between HEAD and the work tree.
pub fn diff_file(repo: &Repository, path: &str) -> Result<String> {
    let work_tree = repo.work_tree.as_deref().context("bare repository has no work tree")?;
    let full = work_tree.join(path);

    let old = match blob_at_head(repo, path)? {
        Some(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        None => String::new(),
    };
    let new = if full.is_file() {
        String::from_utf8_lossy(&fs::read(&full)?).to_string()
    } else {
        String::new()
    };

    let old_path = if old.is_empty() { "/dev/null" } else { path };
    let new_path = if new.is_empty() { "/dev/null" } else { path };
    Ok(unified_diff_histogram_with_prefix_and_funcname(
        &old, &new, old_path, new_path, 3, 0, "a/", "b/", None, false,
    ))
}

/// Per-file patches between a commit and its parent.
pub fn diff_commit(repo: &Repository, sha: &str) -> Result<Vec<FileDiff>> {
    let oid: ObjectId = sha.parse().with_context(|| format!("bad commit {sha}"))?;
    let new_tree = commit_tree(repo, sha)?;
    let old_tree = match parent_of(repo, &oid)? {
        Some(parent) => Some(commit_tree(repo, &parent.to_string())?),
        None => None,
    };
    Ok(patch_entries(repo, old_tree.as_ref(), &new_tree)?)
}

/// Files that differ between two commits.
pub fn changed_files(repo: &Repository, base: &str, head: &str) -> Result<Vec<FileStatus>> {
    let old_tree = commit_tree(repo, base)?;
    let new_tree = commit_tree(repo, head)?;
    Ok(diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "")?
        .iter()
        .map(|e| FileStatus {
            path: e.path().to_string(),
            status: status_name(&e.status).to_string(),
            staged: false,
            unstaged: false,
        })
        .collect())
}

/// Restore one path to its content at HEAD.
pub fn reset_file_to_head(repo: &Repository, path: &str) -> Result<()> {
    let mut index = repo.load_index()?;
    let entry = index
        .entries
        .iter()
        .find(|e| String::from_utf8_lossy(&e.path) == path)
        .cloned()
        .ok_or_else(|| anyhow!("{path} is not tracked"))?;

    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let full = work_tree.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    let blob = repo.odb.read(&entry.oid)?;
    fs::write(&full, &blob.data)?;
    if entry.mode == MODE_EXECUTABLE {
        set_executable(&full)?;
    }
    index.add_or_replace(entry);
    write_index_clean(repo, &mut index)?;
    Ok(())
}

/// Hard reset to `sha`: rewrite the work tree, the index and the ref.
pub fn reset_hard_to(repo: &Repository, sha: &str) -> Result<String> {
    let oid: ObjectId = sha.parse().with_context(|| format!("bad commit {sha}"))?;
    let tree = commit_tree(repo, sha)?;
    let work_tree = repo.work_tree.as_deref().context("bare repository has no work tree")?;

    let previous = index_paths(&repo.load_index()?);
    let entries = flatten_tree(&repo.odb, tree)?;

    let mut index = repo.load_index()?;
    index.entries.clear();
    let mut keep: BTreeSet<String> = BTreeSet::new();

    for (rel, mode, entry_oid) in &entries {
        let blob = repo.odb.read(entry_oid)?;
        let full = work_tree.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, &blob.data)?;
        if *mode == MODE_EXECUTABLE {
            set_executable(&full)?;
        }
        let stat = fs::metadata(&full)?;
        index.add_or_replace(make_index_entry(rel, *mode, *entry_oid, &stat));
        keep.insert(rel.clone());
    }

    // Remove files the target commit does not contain.
    for rel in previous {
        if !keep.contains(&rel) {
            let _ = fs::remove_file(work_tree.join(&rel));
        }
    }

    write_index_clean(repo, &mut index)?;
    let target =
        grit_lib::refs::read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_string());
    grit_lib::refs::write_ref(&repo.git_dir, &target, &oid)?;
    Ok(oid.to_string())
}

/// Put HEAD on `branch` pointing at `sha`, and materialise the work tree.
///
/// HEAD is written as a symbolic ref rather than left detached, because a
/// clone that lands on a detached HEAD cannot commit or pull without the user
/// first noticing.
pub fn checkout_branch(repo: &Repository, branch: &str, sha: &str) -> Result<String> {
    let refname = format!("refs/heads/{branch}");
    grit_lib::refs::write_symbolic_ref(&repo.git_dir, "HEAD", &refname)?;
    reset_hard_to(repo, sha)
}

/// Discard all work-tree changes and go back to HEAD.
pub fn force_checkout_head(repo: &Repository) -> Result<String> {
    let sha = head_sha(repo).ok_or_else(|| anyhow!("HEAD does not point at a commit"))?;
    reset_hard_to(repo, &sha)
}

/// Resolve a ref name (or a raw SHA) to a commit SHA.
///
/// `resolve_ref` already falls back to `packed-refs`, which is what the app's
/// hand-rolled reader had to reimplement — and had to get subtly wrong for
/// packed refs it had never seen on device.
pub fn resolve_ref(repo: &Repository, refname: &str) -> Result<String> {
    // A full SHA is not a ref; callers legitimately pass both.
    if refname.len() == 40 && refname.chars().all(|c| c.is_ascii_hexdigit()) {
        let oid: ObjectId = refname
            .parse()
            .with_context(|| format!("bad object id {refname}"))?;
        // Confirm it exists, so callers never get a plausible-looking SHA for an
        // object that is not there.
        repo.odb.read(&oid)?;
        return Ok(oid.to_string());
    }
    let oid = grit_lib::refs::resolve_ref(&repo.git_dir, refname)
        .with_context(|| format!("cannot resolve {refname}"))?;
    Ok(oid.to_string())
}

/// Every tracked path at `sha`, repo-relative and sorted. `git ls-tree -r`.
pub fn list_files(repo: &Repository, sha: &str) -> Result<Vec<String>> {
    let tree = commit_tree(repo, sha)?;
    let mut paths: Vec<String> = flatten_tree(&repo.odb, tree)?
        .into_iter()
        .map(|(rel, _, _)| rel)
        .collect();
    // Sorted so callers get a stable order without re-sorting on every call.
    paths.sort();
    Ok(paths)
}

/// Stage a single path, recording a deletion when the file is gone.
///
/// `git add -- <path>`: the file may be modified, new, or removed, and all
/// three have to reach the index or the commit records the wrong thing.
pub fn stage_file(repo: &Repository, path: &str) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("bare repository has no work tree")?;
    let full = work_tree.join(path);
    let mut index = repo.load_index()?;

    if !full.exists() {
        index.remove(path.as_bytes());
        return write_index_clean(repo, &mut index);
    }

    // Follow symlinks: git records the target as a blob, not the link itself.
    let meta = fs::symlink_metadata(&full)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&full)?;
        let blob = repo.odb.write(ObjectKind::Blob, target.to_string_lossy().as_bytes())?;
        let mut entry = make_index_entry(path, 0o120000, blob, &meta);
        entry.mode = 0o120000;
        index.add_or_replace(entry);
        return write_index_clean(repo, &mut index);
    }

    let mode = if meta.permissions().mode() & 0o111 != 0 {
        MODE_EXECUTABLE
    } else {
        MODE_REGULAR
    };
    let data = fs::read(&full)?;
    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    index.add_or_replace(make_index_entry(path, mode, oid, &meta));
    write_index_clean(repo, &mut index)
}

/// Fast-forward HEAD and the work tree to `tip`.
///
/// Fails rather than merging when the histories have diverged, which is what
/// `git merge --ff-only` does and what the pull coordinator relies on: it
/// decides up front whether a merge is needed, and a silent merge here would
/// take that decision away from it.
pub fn fast_forward_to(repo: &Repository, tip: &str) -> Result<String> {
    let head = head_sha(repo).ok_or_else(|| anyhow!("HEAD does not point at a commit"))?;
    if head == tip {
        // Already there. Still rewrite the work tree: a pull that finds HEAD
        // equal to the tip may still be syncing files an earlier merge left behind.
        return reset_hard_to(repo, tip);
    }
    let head_oid: ObjectId = head.parse().context("bad HEAD sha")?;
    let tip_oid: ObjectId = tip.parse().with_context(|| format!("bad commit {tip}"))?;
    if !grit_is_ancestor(&repo, head_oid, tip_oid)? {
        bail!("not a fast-forward: {head} is not an ancestor of {tip}");
    }
    reset_hard_to(repo, tip)
}
/// Configured remotes with their fetch URLs, read from `.git/config`.
pub fn list_remotes(repo: &Repository) -> Result<Vec<RemoteInfo>> {
    let path = repo.git_dir.join("config");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let file = grit_lib::config::ConfigFile::parse(
        &path,
        &content,
        grit_lib::config::ConfigScope::Local,
    )?;
    let mut set = grit_lib::config::ConfigSet::new();
    set.merge(&file);

    // `ConfigEntry::key` is already canonical: `section.subsection.name`.
    let mut names: Vec<String> = Vec::new();
    for entry in set.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else { continue };
        let Some((name, leaf)) = rest.rsplit_once('.') else { continue };
        if leaf != "url" || name.is_empty() || names.iter().any(|n| n == name) {
            continue;
        }
        names.push(name.to_string());
    }

    Ok(names
        .into_iter()
        .map(|name| RemoteInfo {
            url: set.get(&format!("remote.{name}.url")).unwrap_or_default(),
            name,
        })
        .collect())
}

/// Commits HEAD is ahead of `remote_tip`.
pub fn ahead_count(repo: &Repository, remote_tip: &str) -> Result<u32> {
    let local: ObjectId = head_sha(repo).ok_or_else(|| anyhow!("unborn HEAD"))?.parse()?;
    let other: ObjectId = remote_tip.parse().with_context(|| format!("bad tip {remote_tip}"))?;
    let (ahead, _) = count_symmetric_ahead_behind(repo, local, other)?;
    Ok(ahead as u32)
}

/// Whether `a` is an ancestor of `b`.
pub fn is_ancestor(repo: &Repository, a: &str, b: &str) -> Result<bool> {
    let ancestor: ObjectId = a.parse().with_context(|| format!("bad commit {a}"))?;
    let descendant: ObjectId = b.parse().with_context(|| format!("bad commit {b}"))?;
    Ok(grit_is_ancestor(repo, ancestor, descendant)?)
}

/// Best common ancestor of two commits, `None` when histories are unrelated.
pub fn merge_base(repo: &Repository, a: &str, b: &str) -> Result<Option<String>> {
    let left: ObjectId = a.parse().with_context(|| format!("bad commit {a}"))?;
    let right: ObjectId = b.parse().with_context(|| format!("bad commit {b}"))?;
    Ok(merge_bases_octopus(repo, &[left, right])?.first().map(|o| o.to_string()))
}

/// Text of `path` as of commit `sha`.
pub fn blob_text(repo: &Repository, sha: &str, path: &str) -> Result<Option<String>> {
    let tree = commit_tree(repo, sha)?;
    let entries = flatten_tree(&repo.odb, tree)?;
    let Some((_, _, oid)) = entries.iter().find(|(rel, _, _)| rel == path) else {
        return Ok(None);
    };
    let object = repo.odb.read(oid)?;
    if object.kind != ObjectKind::Blob {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&object.data).to_string()))
}

/// Differences between the index and the work tree, without the porcelain
/// layer — used to verify the index write actually landed.
pub fn unstaged_entries(repo: &Repository) -> Result<Vec<FileStatus>> {
    let index = repo.load_index()?;
    let work_tree = match repo.work_tree.as_deref() {
        Some(wt) => wt,
        None => return Ok(Vec::new()),
    };
    Ok(diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?
        .iter()
        .map(|e| FileStatus {
            path: e.path().to_string(),
            status: status_name(&e.status).to_string(),
            staged: false,
            unstaged: false,
        })
        .collect())
}

/// Staged differences between the index and the HEAD tree.
pub fn staged_entries(repo: &Repository) -> Result<Vec<FileStatus>> {
    let index = repo.load_index()?;
    let head_tree = head_sha(repo).and_then(|sha| commit_tree(repo, &sha).ok());
    Ok(diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?
        .iter()
        .map(|e| FileStatus {
            path: e.path().to_string(),
            status: status_name(&e.status).to_string(),
            staged: false,
            unstaged: false,
        })
        .collect())
}

// MARK: - internals

/// Write the index, dropping any cached tree extension first.
///
/// `grit_lib` loads the `TREE` (cache-tree) extension into `Index::cache_tree` but
/// neither `add_or_replace` nor a direct `entries` edit invalidates it, so
/// `write_index` serialises a tree cache that still describes the pre-mutation
/// index. Git trusts a valid cache-tree node and skips re-deriving that subtree,
/// so the stale node makes it pair index entries against the wrong tree entries:
/// staging a deletion under `notes/sub/` made `git status` report `D run.sh`
/// instead of `D notes/sub/two.md`. Dropping the extension is always safe — git
/// falls back to re-deriving the trees from the entries on the next read.
fn write_index_clean(repo: &Repository, index: &mut Index) -> Result<()> {
    index.cache_tree = None;
    index.cache_tree_root = None;
    repo.write_index(index)?;
    Ok(())
}

fn status_name(status: &DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "added",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::Renamed => "renamed",
        DiffStatus::Copied => "copied",
        DiffStatus::TypeChanged => "type_changed",
        DiffStatus::Unmerged => "unmerged",
    }
}

fn index_paths(index: &Index) -> BTreeSet<String> {
    index.entries.iter().map(|e| String::from_utf8_lossy(&e.path).to_string()).collect()
}

/// Build unified patches for every changed file between two trees.
fn patch_entries(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<Vec<FileDiff>> {
    let entries = diff_trees(&repo.odb, old_tree, Some(new_tree), "")?;
    let mut out = Vec::new();
    for entry in &entries {
        let path = entry.path().to_string();
        let old_text = read_blob_text(repo, &entry.old_oid);
        let new_text = read_blob_text(repo, &entry.new_oid);
        let old_path = if entry.old_oid == zero_oid() { "/dev/null" } else { path.as_str() };
        let new_path = if entry.new_oid == zero_oid() { "/dev/null" } else { path.as_str() };
        let patch = unified_diff_histogram_with_prefix_and_funcname(
            &old_text, &new_text, old_path, new_path, 3, 0, "a/", "b/", None, false,
        );
        out.push(FileDiff { path, patch });
    }
    Ok(out)
}

fn read_blob_text(repo: &Repository, oid: &ObjectId) -> String {
    if *oid == zero_oid() {
        return String::new();
    }
    match repo.odb.read(oid) {
        Ok(o) if o.kind == ObjectKind::Blob => String::from_utf8_lossy(&o.data).to_string(),
        _ => String::new(),
    }
}

fn zero_oid() -> ObjectId {
    ObjectId::zero()
}

/// One commit as the app's timeline needs it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitRecord {
    pub hash: String,
    /// Author timestamp, in seconds since the epoch.
    ///
    /// Author time, not committer time: this is what the timeline UI sorts and
    /// filters on, and a rebase or amend moves the committer date while leaving
    /// the author's intent where it was. See the alignment note in ops::log.
    pub author_time: i64,
    /// First line of the message, trimmed.
    pub subject: String,
    /// Full message, including the subject line.
    pub message: String,
}

/// Commits reachable from HEAD, newest first, ordered by AUTHOR date.
///
/// `OrderingMode::AuthorDateWalk` is git's `--author-date-order`, so the
/// traversal matches what `git log --author-date-order` would produce. The
/// plain `Default` mode would order by committer date, which is the thing we
/// are deliberately not using.
///
/// `limit` truncates AFTER the sort, so the caller gets the newest `limit`
/// commits rather than the first `limit` walked.
pub fn log(repo: &Repository, limit: usize) -> Result<Vec<CommitRecord>> {
    let options = RevListOptions {
        ordering: OrderingMode::AuthorDateWalk,
        ..Default::default()
    };
    let result = rev_list(repo, &["HEAD".to_string()], &[], &options)?;

    let mut commits = Vec::with_capacity(result.commits.len());
    for oid in &result.commits {
        let object = repo.odb.read(oid)?;
        if object.kind != ObjectKind::Commit {
            continue;
        }
        let data = parse_commit(&object.data)?;
        commits.push(CommitRecord {
            hash: oid.to_string(),
            author_time: identity_time(&data),
            subject: subject_of(&data.message),
            message: data.message.clone(),
        });
    }

    // rev_list already returned author-date order, but sorting again makes the
    // guarantee ours rather than the caller's, and keeps the Swift side from
    // having to re-sort the whole history. Stable, so commits sharing an author
    // timestamp keep walk order instead of shuffling between calls.
    commits.sort_by(|a, b| b.author_time.cmp(&a.author_time));
    // 0 means "no cap": the Swift side filters the range itself and passes 0
    // when it does not know how many commits are in scope. truncate(0) would
    // silently empty the result, so treat it as unlimited rather than literal.
    if limit > 0 {
        commits.truncate(limit);
    }
    Ok(commits)
}

/// The epoch seconds in an identity line: `Name <email> 1700000000 +0800`.
///
/// The timestamp is the second-to-last whitespace-separated field. Returns 0
/// when the line is too short to carry one, which sorts the commit last rather
/// than failing the whole walk.
fn identity_time(data: &CommitData) -> i64 {
    let raw = String::from_utf8_lossy(&data.author_raw);
    let fields: Vec<&str> = raw.split_whitespace().collect();
    if fields.len() < 2 {
        return 0;
    }
    fields[fields.len() - 2].parse().unwrap_or(0)
}

/// The subject line: the first paragraph of the message, up to the first blank
/// line, joined with single spaces.
///
/// This is git's own definition of `%s` — a commit whose message wraps across
/// three lines still has one subject, and callers that only want a title should
/// not have to re-derive that.
fn subject_of(message: &str) -> String {
    let first_paragraph: Vec<&str> = message
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .collect();
    first_paragraph
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn commit_tree(repo: &Repository, sha: &str) -> Result<ObjectId> {
    let oid: ObjectId = sha.parse().with_context(|| format!("bad commit {sha}"))?;
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        bail!("{sha} is not a commit");
    }
    Ok(parse_commit(&object.data)?.tree)
}

fn parent_of(repo: &Repository, oid: &ObjectId) -> Result<Option<ObjectId>> {
    let object = repo.odb.read(oid)?;
    Ok(parse_commit(&object.data)?.parents.first().copied())
}

fn blob_at_head(repo: &Repository, path: &str) -> Result<Option<Vec<u8>>> {
    let Some(sha) = head_sha(repo) else { return Ok(None) };
    let tree = commit_tree(repo, &sha)?;
    let entries = flatten_tree(&repo.odb, tree)?;
    let Some((_, _, oid)) = entries.iter().find(|(rel, _, _)| rel == path) else {
        return Ok(None);
    };
    Ok(Some(repo.odb.read(oid)?.data))
}

/// Flatten a tree into (relative path, mode, oid), depth first, sorted by path.
fn flatten_tree(odb: &grit_lib::odb::Odb, tree: ObjectId) -> Result<Vec<(String, u32, ObjectId)>> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), tree)];
    while let Some((prefix, oid)) = stack.pop() {
        let object = odb.read(&oid)?;
        if object.kind != ObjectKind::Tree {
            continue;
        }
        for entry in parse_tree(&object.data)? {
            let name = String::from_utf8_lossy(&entry.name).to_string();
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if entry.mode == 0o040000 {
                stack.push((path, entry.oid));
            } else {
                out.push((path, entry.mode, entry.oid));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn make_index_entry(path: &str, mode: u32, oid: ObjectId, stat: &fs::Metadata) -> IndexEntry {
    IndexEntry {
        ctime_sec: stat.ctime() as u32,
        ctime_nsec: stat.ctime_nsec() as u32,
        mtime_sec: stat.mtime() as u32,
        mtime_nsec: stat.mtime_nsec() as u32,
        dev: stat.dev() as u32,
        ino: stat.ino() as u32,
        mode,
        uid: 0,
        gid: 0,
        size: stat.len().min(u64::from(u32::MAX)) as u32,
        oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

/// Stage one work-tree file: write the blob, add the index entry.
fn add_path(repo: &Repository, index: &mut Index, full: &Path) -> Result<()> {
    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let rel = full
        .strip_prefix(work_tree.canonicalize()?)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| full.to_string_lossy().to_string());

    let data = fs::read(full)?;
    let stat = fs::metadata(full)?;
    let mode =
        if stat.permissions().mode() & 0o111 != 0 { MODE_EXECUTABLE } else { MODE_REGULAR };
    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    index.add_or_replace(make_index_entry(&rel, mode, oid, &stat));
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// `Name <email> <unix-seconds> +0000`
fn format_ident(name: &str, email: &str) -> String {
    let secs =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{name} <{email}> {secs} +0000")
}

/// Reflogs are advisory; a failure here must not fail the commit.
fn append_reflog(repo: &Repository, refname: &str, oid: &ObjectId) {
    let path = repo.git_dir.join("logs").join(refname);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{} {} NeuraScribe <app@neurascribe.local> 0 +0000", zero_oid(), oid);
    }
}

/// Path -> oid map for a commit, used by tests and diagnostics.
pub fn tree_map(repo: &Repository, sha: &str) -> Result<BTreeMap<String, ObjectId>> {
    let tree = commit_tree(repo, sha)?;
    Ok(flatten_tree(&repo.odb, tree)?.into_iter().map(|(p, _, o)| (p, o)).collect())
}
