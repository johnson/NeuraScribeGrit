//! Submodule operations assembled from `grit-lib` primitives.
//!
//! grit ships no `submodule` porcelain, so the four things the app needs are
//! built here from the pieces that do exist:
//!
//! - `.gitmodules` is read and written with `ConfigFile`, the same parser git
//!   uses for it, so quoting and subsection names round-trip correctly. The
//!   previous Swift implementation hand-parsed lines and rewrote the file with
//!   a regex, which cannot represent a value containing `]` or a subsection
//!   whose name needs quoting.
//! - the recorded commit lives in the index as a gitlink entry
//!   (`MODE_GITLINK`), never in `.gitmodules`.
//! - a submodule's own repository is opened normally once its work tree
//!   exists, so `reset_hard_to` does the checkout.
//!
//! Cloning is deliberately *not* here: it needs the HTTP client, which lives in
//! Swift. `record_submodule` therefore only writes the registration and the
//! gitlink after the clone has already landed.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::index::{Index, IndexEntry, MODE_GITLINK};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::ops;

/// One entry from `.gitmodules`, with the state the app's UI shows.
#[derive(Debug, Clone, Serialize)]
pub struct SubmoduleInfo {
    /// `submodule.<name>` section name.
    pub name: String,
    /// Work-tree path, relative to the repository root.
    pub path: String,
    /// Clone URL, as written in `.gitmodules`.
    pub url: String,
    /// The commit the superproject's index records for this path.
    pub indexed_sha: Option<String>,
    /// The commit the submodule's work tree is actually on.
    pub head_sha: Option<String>,
    /// Whether the submodule has a checked-out work tree with a usable git dir.
    pub initialized: bool,
}

/// Normalize a `.gitmodules` path: forward slashes, no trailing slash.
fn clean_path(raw: &str) -> String {
    raw.replace('\\', "/").trim_end_matches('/').to_string()
}

/// Read `.gitmodules` into `(name -> (path, url))`.
///
/// Falls back to the stage-0 blob when the work-tree file is absent (sparse
/// checkout, or a clone with no working tree), which is what git does.
fn read_registrations(repo: &Repository) -> Result<BTreeMap<String, (String, String)>> {
    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let gitmodules_path = work_tree.join(".gitmodules");

    let content = if gitmodules_path.exists() {
        fs::read_to_string(&gitmodules_path)
            .with_context(|| format!("reading {}", gitmodules_path.display()))?
    } else {
        let index = repo.load_index()?;
        let Some(entry) = index.get(b".gitmodules", 0) else {
            return Ok(BTreeMap::new());
        };
        let obj = repo.odb.read(&entry.oid)?;
        String::from_utf8(obj.data).context(".gitmodules is not valid UTF-8")?
    };

    let config = ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)
        .context("parsing .gitmodules")?;

    #[derive(Default)]
    struct Fields {
        path: Option<String>,
        url: Option<String>,
    }
    let mut by_name: BTreeMap<String, Fields> = BTreeMap::new();
    for entry in &config.entries {
        let Some(rest) = entry.key.strip_prefix("submodule.") else {
            continue;
        };
        // rfind, not find: a subsection name may itself contain dots.
        let Some(dot) = rest.rfind('.') else { continue };
        let (name, var) = (&rest[..dot], &rest[dot + 1..]);
        let slot = by_name.entry(name.to_string()).or_default();
        match var {
            "path" => slot.path = entry.value.clone(),
            "url" => slot.url = entry.value.clone(),
            _ => {}
        }
    }

    Ok(by_name
        .into_iter()
        .filter_map(|(name, f)| match (f.path, f.url) {
            // git treats a registration with no url as inactive; so do we.
            (Some(p), Some(u)) => {
                let p = clean_path(&p);
                if p.is_empty() { None } else { Some((name, (p, u))) }
            }
            _ => None,
        })
        .collect())
}

/// True when the submodule's work tree is checked out with a usable git dir.
///
/// A checked-out submodule has a `.git` *file* pointing into
/// `.git/modules/<name>`, so this also confirms the target still exists —
/// otherwise a deinitialized submodule would look present.
fn is_initialized(repo: &Repository, path: &str) -> bool {
    let Some(work_tree) = repo.work_tree.as_deref() else { return false };
    let dir = work_tree.join(path);
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return true;
    }
    if !dot_git.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&dot_git) else { return false };
    let Some(target) = text
        .trim()
        .rsplitn(2, ' ')
        .next()
        .map(|t| t.trim().trim_start_matches("gitdir:").trim())
    else {
        return false;
    };
    // git writes the target relative to the directory holding the .git file
    // whenever the submodule lives inside the superproject's work tree, so a
    // relative path resolves against that directory — not the process cwd.
    let target = Path::new(target);
    let resolved = if target.is_absolute() { target.to_path_buf() } else { dir.join(target) };
    resolved.join("HEAD").exists()
}

/// Every submodule registered in `.gitmodules`, sorted by work-tree path.
pub fn submodules(repo: &Repository) -> Result<Vec<SubmoduleInfo>> {
    let index = repo.load_index()?;
    let mut out: Vec<SubmoduleInfo> = Vec::new();

    for (name, (path, url)) in read_registrations(repo)? {
        let indexed_sha = index
            .get(path.as_bytes(), 0)
            .filter(|e| e.mode == MODE_GITLINK)
            .map(|e| e.oid.to_string());

        let initialized = is_initialized(repo, &path);
        let head_sha = if initialized {
            let full = repo.work_tree.as_deref().map(|w| w.join(&path)).unwrap_or_default();
            // A half-initialized submodule is not an error; it just has no HEAD
            // we can report.
            Repository::discover(Some(&full)).ok().as_ref().and_then(ops::head_sha)
        } else {
            None
        };

        out.push(SubmoduleInfo { name, path, url, indexed_sha, head_sha, initialized });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The commit the superproject records for a submodule, or an error if the
/// index has no gitlink for it (never added, or already removed).
pub fn indexed_sha(repo: &Repository, path: &str) -> Result<String> {
    let index = repo.load_index()?;
    let entry = index
        .get(clean_path(path).as_bytes(), 0)
        .filter(|e| e.mode == MODE_GITLINK)
        .ok_or_else(|| anyhow!("{path} is not a registered submodule"))?;
    Ok(entry.oid.to_string())
}

/// The `.gitmodules` section name registered for `path`.
pub fn name_for_path(repo: &Repository, path: &str) -> Result<String> {
    let path = clean_path(path);
    read_registrations(repo)?
        .into_iter()
        .find(|(_, (p, _))| *p == path)
        .map(|(name, _)| name)
        .ok_or_else(|| anyhow!("{path} is not a submodule"))
}

/// Record a submodule: write its `.gitmodules` section and stage the gitlink.
///
/// `sha` must be a commit that already exists in the submodule's own object
/// store. This writes the registration only — the caller has cloned.
pub fn record_submodule(repo: &Repository, name: &str, path: &str, url: &str, sha: &str) -> Result<()> {
    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let path = clean_path(path);
    let oid: ObjectId = sha.parse().with_context(|| format!("bad commit {sha}"))?;

    // 1. `.gitmodules`, through git's own config writer so the section order and
    //    quoting come out the way git would write them.
    let gitmodules_path = work_tree.join(".gitmodules");
    let mut config = if gitmodules_path.exists() {
        let content = fs::read_to_string(&gitmodules_path)?;
        ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)
            .context("parsing .gitmodules")?
    } else {
        ConfigFile::parse(&gitmodules_path, "", ConfigScope::Local)?
    };
    // set() replaces in place when the key exists, so this is also the update
    // path — no read-merge-write of the raw text.
    config.set(&format!("submodule.{name}.path"), &path)?;
    config.set(&format!("submodule.{name}.url"), url)?;
    config.write()?;

    // 2. The gitlink. A gitlink names a commit in another repository rather
    //    than a file on disk, so the stat fields stay zero.
    let mut index = repo.load_index()?;
    index.add_or_replace(gitlink_entry(&path, oid));
    write_index(repo, &mut index)?;

    // 3. `.gitmodules` is a tracked file and has to be staged with the rest,
    //    or git reports the registration as untracked and a commit of the
    //    gitlink alone would produce a tree with no .gitmodules in it.
    ops::stage_file(repo, ".gitmodules")?;
    Ok(())
}

/// Build a gitlink index entry.
fn gitlink_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_GITLINK,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

/// Remove a submodule: drop the registration, the gitlink, the work tree and
/// the submodule's own repository.
pub fn remove_submodule(repo: &Repository, path: &str) -> Result<()> {
    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let path = clean_path(path);
    let name = name_for_path(repo, &path)?;

    // 1. `.gitmodules`
    let gitmodules_path = work_tree.join(".gitmodules");
    if gitmodules_path.exists() {
        let content = fs::read_to_string(&gitmodules_path)?;
        let mut config =
            ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local).context("parsing .gitmodules")?;
        if config.remove_section(&format!("submodule.{name}"))? {
            config.write()?;
        }
        // git removes the file when the last submodule goes.
        if fs::read_to_string(&gitmodules_path).unwrap_or_default().trim().is_empty() {
            let _ = fs::remove_file(&gitmodules_path);
        }
    }

    // 2. The gitlink
    let mut index = repo.load_index()?;
    index.remove(path.as_bytes());
    // `.gitmodules` is itself a tracked file, and reads fall back to its index
    // blob when the work-tree file is gone. Leaving that blob behind would
    // resurrect the registration we just removed, so drop the entry when the
    // file went away — which is what `git rm` does.
    if !gitmodules_path.exists() {
        index.remove(b".gitmodules");
    }
    write_index(repo, &mut index)?;

    // 3. The work tree, then the submodule's own repository. Work tree first:
    //    it holds the gitfile pointing at the git dir, so the other order
    //    leaves a dangling pointer behind if this fails partway.
    let full = work_tree.join(&path);
    if full.exists() {
        fs::remove_dir_all(&full).with_context(|| format!("removing {}", full.display()))?;
    }
    let modules = repo.git_dir.to_path_buf().join("modules").join(&name);
    if modules.exists() {
        fs::remove_dir_all(&modules).with_context(|| format!("removing {}", modules.display()))?;
    }
    Ok(())
}

/// Check the submodule's work tree out at the commit the superproject records.
///
/// This is the whole of `submodule update` once the clone exists: the
/// superproject says *which* commit, and the submodule's own repository does
/// the checkout. Local edits inside the submodule are discarded, as in git.
pub fn checkout_submodule_to(repo: &Repository, path: &str, sha: &str) -> Result<String> {
    let work_tree = repo.work_tree.as_deref().context("bare repository")?;
    let path = clean_path(path);
    let _name = name_for_path(repo, &path)?;
    let oid: ObjectId = sha.parse().with_context(|| format!("bad commit {sha}"))?;

    let target = work_tree.join(&path);
    if !target.exists() {
        bail!("submodule {path} is not initialized; clone it before updating");
    }

    let sub = Repository::discover(Some(&target))
        .with_context(|| format!("{path} is not a git repository yet"))?;
    // The commit has to be in the submodule's own object store. Callers clone
    // (or fetch into) first; a missing commit is a real error, not a silent
    // no-op that would leave the submodule at the wrong revision.
    sub.odb.read(&oid).with_context(|| {
        format!("commit {sha} is not in submodule {path}; fetch it first")
    })?;
    ops::reset_hard_to(&sub, sha)
}

/// Write the index back, clearing the cache-tree first.
///
/// A stale cache-tree makes git pair index entries with the wrong tree nodes,
/// which is the first defect this shim found in grit; never write one back.
fn write_index(repo: &Repository, index: &mut Index) -> Result<()> {
    index.clear_cache_tree();
    repo.write_index(index)?;
    Ok(())
}