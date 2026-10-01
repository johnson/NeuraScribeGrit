//! `git merge`, assembled from grit three-way merge plus the ref and index
//! plumbing this crate already has.
//!
//! grit-lib ships `merge_trees_three_way`, which produces a merged index and
//! the conflict-marker blobs for whatever did not merge cleanly. What is
//! missing is the command around it: the fast-forward decision, refusing on a
//! work tree that would be clobbered, writing the result out, and the merge
//! commit. That is this file.
//!
//! The states match git, because the on-disk markers do:
//!
//! - A clean merge creates the commit and moves the branch.
//! - A conflicted merge writes `MERGE_HEAD` and the conflict markers, leaves
//!   HEAD where it was, and returns the conflicted paths. The repository is
//!   then mid-merge, which `git status` agrees with.
//! - `finish_merge` makes the commit once the conflicts are staged away.
//! - `abort_merge` puts everything back.

use std::collections::BTreeSet;
use std::fs;

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::index::{Index, MODE_EXECUTABLE};

use grit_lib::merge_file::{ConflictStyle, MergeFavor};
use grit_lib::merge_trees::{
    merge_trees_three_way, TheirsConflictLabel, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::write_tree::write_tree_from_index;

use crate::ops::{self, MODE_GITLINK};
use crate::types::MergeOutcome;

/// Written while a merge is waiting on the user, exactly as git does it.
const MERGE_HEAD: &str = "MERGE_HEAD";
/// Where the branch was before the merge started, so abort has somewhere to go.
const ORIG_HEAD: &str = "ORIG_HEAD";

/// Paths the work tree holds changes in right now, tracked or not.
fn dirty_paths(repo: &Repository) -> Result<BTreeSet<String>> {
    let opts = grit_lib::porcelain::status::StatusOptions {
        untracked: grit_lib::porcelain::status::UntrackedMode::All,
        ignored: grit_lib::porcelain::status::IgnoredMode::No,
        renames: None,
        pathspecs: Vec::new(),
        ahead_behind: false,
    };
    let model = grit_lib::porcelain::status::status(repo, &opts, &mut NullProgress)?;
    let mut paths = BTreeSet::new();
    for entry in model.unstaged.iter().chain(model.staged.iter()) {
        paths.insert(entry.path().to_string());
    }
    for path in &model.untracked {
        paths.insert(path.clone());
    }
    Ok(paths)
}

/// Paths that differ between two commits trees.
fn differing_paths(repo: &Repository, from: &str, to: &str) -> Result<BTreeSet<String>> {
    let left = ops::tree_map(repo, from)?;
    let right = ops::tree_map(repo, to)?;
    let mut paths = BTreeSet::new();
    for (path, oid) in &left {
        if right.get(path) != Some(oid) {
            paths.insert(path.clone());
        }
    }
    for (path, _oid) in &right {
        if !left.contains_key(path) {
            paths.insert(path.clone());
        }
    }
    Ok(paths)
}

fn abbrev(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Refuse before anything is written, naming the files, the way git does.
fn refuse_if_dirty_overlap(
    repo: &Repository,
    touched: &BTreeSet<String>,
) -> Result<()> {
    let dirty = dirty_paths(repo)?;
    let clash: Vec<String> = touched.intersection(&dirty).cloned().collect();
    if clash.is_empty() {
        return Ok(());
    }
    bail!(
        "your local changes to the following files would be overwritten by merge: {}",
        clash.join(", ")
    )
}

/// Write a merged index out to the work tree and to the index file.
///
/// Stage-0 entries are written from the object store; entries left unmerged
/// are skipped, because their work tree content is the conflict marker the
/// caller writes separately.
fn apply_index_to_worktree(repo: &Repository, index: &mut Index) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("bare repository has no work tree")?;
    let previous = ops::index_paths(index);
    let mut keep = BTreeSet::new();

    for entry in index.entries.iter_mut() {
        if entry.stage() != 0 {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path).to_string();
        let full = work_tree.join(&rel);
        if entry.mode == MODE_GITLINK {
            // A submodule pointer: nothing of ours to write.
            keep.insert(rel);
            continue;
        }
        let (mode, oid) = (entry.mode, entry.oid);
        let data = repo.odb.read(&oid)?.data;
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, &data)?;
        if mode == MODE_EXECUTABLE {
            ops::set_executable(&full)?;
        }
        *entry = ops::make_index_entry(&rel, mode, oid, &fs::metadata(&full)?);
        keep.insert(rel);
    }

    // A merge can remove a path. Only paths that were tracked may go: an
    // untracked file the user just made is not ours to delete.
    for rel in &previous {
        if !keep.contains(rel) {
            let _ = fs::remove_file(work_tree.join(rel));
        }
    }
    ops::write_index_clean(repo, index)?;
    Ok(())
}

/// A literal single quote, spelled as a const so the subject strings below
/// stay readable.
const QUOTE: char = '\'';

/// Wrap in the single quotes git puts around branch names and short ids in
/// commit subjects.
fn quoted(value: &str) -> String {
    format!("{}{}{}", QUOTE, value, QUOTE)
}

/// The default subject git would use for this merge.
fn default_subject(repo: &Repository, spec: &str, other_sha: &str) -> String {
    if refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{spec}")).is_ok() {
        format!("Merge branch {}", quoted(spec))
    } else {
        format!("Merge commit {}", quoted(&abbrev(other_sha)))
    }
}
/// Merge `spec` (a branch name or commit id) into the current branch.
///
/// Returns what happened rather than only succeeding or failing: a merge that
/// finds nothing to do is not an error, and one that hits conflicts is a
/// result the caller has to show, not a bare failure.
pub fn merge(repo: &Repository, spec: &str, message: Option<&str>) -> Result<MergeOutcome> {
    let head = ops::head_sha(repo).context("HEAD does not point at a commit")?;
    let (_, other) = refs::resolve_ref_dwim(&repo.git_dir, spec);
    let other = other.with_context(|| format!("cannot resolve {spec}"))?;
    let other_sha = other.to_string();
    let head_oid: ObjectId = head.parse().context("parse HEAD sha")?;

    if other_sha == head || ops::is_ancestor(repo, &other_sha, &head)? {
        // Already contained in HEAD.
        return Ok(MergeOutcome::UpToDate);
    }

    if ops::is_ancestor(repo, &head, &other_sha)? {
        let touched = differing_paths(repo, &head, &other_sha)?;
        refuse_if_dirty_overlap(repo, &touched)?;
        // ORIG_HEAD first: if the checkout that follows fails, abort has
        // somewhere to go back to.
        let _ = refs::write_ref(&repo.git_dir, ORIG_HEAD, &head_oid);
        let landed = ops::fast_forward_to(repo, &other_sha)?;
        return Ok(MergeOutcome::FastForward { head: landed });
    }

    let base = ops::merge_base(repo, &head, &other_sha)?
        .ok_or_else(|| anyhow!("no common ancestor between HEAD and {spec}; they are unrelated histories"))?;

    let ours_label = ops::current_branch(repo).unwrap_or_else(|_| "HEAD".to_string());
    let base_label = abbrev(&base);
    let merged = merge_trees_three_way(
        repo,
        ops::commit_tree(repo, &base)?,
        ops::commit_tree(repo, &head)?,
        ops::commit_tree(repo, &other_sha)?,
        MergeFavor::None,
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation {
            label_ours: &ours_label,
            label_theirs: TheirsConflictLabel::PathUtf8,
            label_base: &base_label,
            style: ConflictStyle::Merge,
            checkout_merge: false,
        },
    )?;

    let mut index = merged.index;
    let conflicted: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| String::from_utf8_lossy(&e.path).to_string())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();

    let touched = differing_paths(repo, &head, &other_sha)?;
    // Checked before anything is written, whatever the outcome turns out to be.
    refuse_if_dirty_overlap(repo, &touched)?;

    if !conflicted.is_empty() {
        apply_index_to_worktree(repo, &mut index)?;
        let work_tree = repo
            .work_tree
            .as_deref()
            .context("bare repository has no work tree")?;
        for (path, oid) in &merged.conflict_content {
            let rel = String::from_utf8_lossy(path).to_string();
            fs::write(work_tree.join(&rel), &repo.odb.read(oid)?.data)?;
        }
        let _ = refs::write_ref(&repo.git_dir, ORIG_HEAD, &head_oid);
        fs::write(repo.git_dir.join(MERGE_HEAD), format!("{other_sha}\n"))?;
        return Ok(MergeOutcome::Conflicted {
            head,
            files: conflicted,
        });
    }

    // Clean: build the commit, write the work tree, then move the ref -- the
    // same order `reset_hard_to` uses, so a merge cannot leave HEAD pointing
    // at content that is not on disk.
    let tree = write_tree_from_index(&repo.odb, &index, "")?;
    let subject = match message {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => default_subject(repo, spec, &other_sha),
    };
    let ident = ops::format_ident("NeuraScribe", "app@neurascribe.local");
    let commit = CommitData {
        tree,
        parents: vec![head_oid, other],
        author: ident.clone(),
        committer: ident.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{subject}\n"),
        raw_message: None,
    };
    let oid = repo.odb.write(ObjectKind::Commit, &serialize_commit(&commit))?;

    let target = refs::read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_string());
    let previous_ref = ops::ref_value(repo, &target);
    let _ = refs::write_ref(&repo.git_dir, ORIG_HEAD, &head_oid);
    refs::write_ref(&repo.git_dir, &target, &oid)?;
    ops::append_reflog(
        repo,
        &target,
        &previous_ref,
        &oid,
        &ops::app_reflog_identity(),
        &format!("merge {spec}: {subject}"),
    );

    apply_index_to_worktree(repo, &mut index)?;
    let _ = fs::remove_file(repo.git_dir.join(MERGE_HEAD));
    Ok(MergeOutcome::Merged {
        head: oid.to_string(),
    })
}

/// Paths still carrying conflict markers, i.e. unmerged index entries.
pub fn unresolved_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.load_index()?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            paths.insert(String::from_utf8_lossy(&entry.path).to_string());
        }
    }
    Ok(paths.into_iter().collect())
}

/// Whether a merge is waiting to be finished.
pub fn merge_in_progress(repo: &Repository) -> bool {
    repo.git_dir.join(MERGE_HEAD).exists()
}

/// Make the merge commit once every conflict is resolved.
///
/// Refuses while anything is still unmerged, naming the files: committing a
/// half-resolved merge would put conflict markers into history.
pub fn finish_merge(repo: &Repository, message: Option<&str>) -> Result<String> {
    let merge_head_path = repo.git_dir.join(MERGE_HEAD);
    let merge_head = fs::read_to_string(&merge_head_path)
        .context("there is no merge in progress")?
        .trim()
        .to_string();
    let unresolved = unresolved_paths(repo)?;
    if !unresolved.is_empty() {
        bail!(
            "these files still have conflicts: {}",
            unresolved.join(", ")
        );
    }
    let head = ops::head_sha(repo).context("HEAD does not point at a commit")?;
    let index = repo.load_index()?;
    let tree = write_tree_from_index(&repo.odb, &index, "")?;
    let subject = match message {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => format!("Merge commit {}", quoted(&abbrev(&merge_head))),
    };
    let ident = ops::format_ident("NeuraScribe", "app@neurascribe.local");
    let commit = CommitData {
        tree,
        parents: vec![head.parse()?, merge_head.parse()?],
        author: ident.clone(),
        committer: ident.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{subject}\n"),
        raw_message: None,
    };
    let oid = repo.odb.write(ObjectKind::Commit, &serialize_commit(&commit))?;
    let target = refs::read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_string());
    let previous_ref = ops::ref_value(repo, &target);
    refs::write_ref(&repo.git_dir, &target, &oid)?;
    ops::append_reflog(
        repo,
        &target,
        &previous_ref,
        &oid,
        &ops::app_reflog_identity(),
        &format!("merge (finish): {subject}"),
    );
    let _ = fs::remove_file(&merge_head_path);
    Ok(oid.to_string())
}

/// Put everything back and forget the merge.
///
/// Goes to ORIG_HEAD, which is where the branch was before the merge started,
/// and hard-resets: the conflicted work tree content is exactly what is being
/// thrown away, and the user asked for it.
pub fn abort_merge(repo: &Repository) -> Result<String> {
    if !merge_in_progress(repo) {
        bail!("there is no merge in progress");
    }
    let origin = refs::resolve_ref(&repo.git_dir, ORIG_HEAD)
        .context("this merge left no ORIG_HEAD to go back to")?
        .to_string();
    let sha = ops::reset_hard_to(repo, &origin)?;
    let _ = fs::remove_file(repo.git_dir.join(MERGE_HEAD));
    Ok(sha)
}

