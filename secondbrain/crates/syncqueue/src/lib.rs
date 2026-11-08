//! Git-backed vault sync with an offline queue.
//!
//! Model: the device's local edits (highlights) are **commits** — that's the offline queue.
//! When a network appears, [`sync_vault`] commits pending edits, pulls (fast-forward, or a
//! merge commit when changes are independent), and pushes. A genuine content conflict on the
//! same lines is **never silently resolved**: the merge is left in the working tree with
//! conflict markers, and the call errors so the caller can surface it.

use anyhow::{bail, Context, Result};
use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{Cred, CredentialType, FetchOptions, PushOptions, RemoteCallbacks, Repository};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    UpToDate,
    FastForwarded,
    Merged,
}

#[derive(Debug)]
pub struct SyncReport {
    pub committed: Option<git2::Oid>,
    pub outcome: SyncOutcome,
    pub pushed: bool,
}

pub struct VaultRepo {
    repo: Repository,
    ssh_key: Option<PathBuf>,
}

impl VaultRepo {
    /// Open the local clone, or clone it from `url` if absent.
    pub fn open_or_clone(
        url: &str,
        local: &Path,
        branch: Option<&str>,
        ssh_key: Option<PathBuf>,
    ) -> Result<Self> {
        let repo = if local.join(".git").exists() {
            Repository::open(local).with_context(|| format!("opening {}", local.display()))?
        } else {
            let mut fo = FetchOptions::new();
            fo.remote_callbacks(callbacks(ssh_key.clone()));
            let mut builder = RepoBuilder::new();
            builder.fetch_options(fo);
            if let Some(b) = branch {
                builder.branch(b);
            }
            builder.clone(url, local).with_context(|| format!("cloning {url}"))?
        };
        Ok(VaultRepo { repo, ssh_key })
    }

    /// The current branch short name (e.g. "main").
    pub fn current_branch(&self) -> Result<String> {
        let head = self.repo.head().context("HEAD")?;
        Ok(head.shorthand().unwrap_or("main").to_string())
    }

    /// Stage all changes and commit. Returns the new commit id, or `None` if the tree was clean.
    pub fn commit_all(&self, message: &str) -> Result<Option<git2::Oid>> {
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree_id = index.write_tree()?;

        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| self.repo.find_commit(oid).ok());
        if let Some(p) = &parent {
            if p.tree_id() == tree_id {
                return Ok(None); // nothing to commit
            }
        }
        let tree = self.repo.find_tree(tree_id)?;
        let sig = self.signature()?;
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
        Ok(Some(oid))
    }

    fn signature(&self) -> Result<git2::Signature<'static>> {
        if let Ok(sig) = self.repo.signature() {
            return Ok(sig);
        }
        Ok(git2::Signature::now(
            "reMarkable second brain",
            "device@secondbrain.local",
        )?)
    }

    fn fetch(&self) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks(self.ssh_key.clone()));
        remote.fetch::<&str>(&[], Some(&mut fo), None).context("git fetch")?;
        Ok(())
    }

    /// Fetch and integrate `origin/<branch>`: fast-forward, merge, or up-to-date.
    pub fn pull(&self, branch: &str) -> Result<SyncOutcome> {
        self.fetch()?;
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let remote_oid = match self.repo.refname_to_id(&remote_ref) {
            Ok(o) => o,
            Err(_) => return Ok(SyncOutcome::UpToDate), // remote branch not present yet
        };
        let annotated = self.repo.find_annotated_commit(remote_oid)?;
        let (analysis, _) = self.repo.merge_analysis(&[&annotated])?;

        if analysis.is_up_to_date() {
            return Ok(SyncOutcome::UpToDate);
        }
        if analysis.is_fast_forward() {
            let local_ref = format!("refs/heads/{branch}");
            match self.repo.find_reference(&local_ref) {
                Ok(mut r) => {
                    r.set_target(remote_oid, "fast-forward")?;
                }
                Err(_) => {
                    self.repo.reference(&local_ref, remote_oid, true, "fast-forward")?;
                }
            }
            self.repo.set_head(&format!("refs/heads/{branch}"))?;
            self.repo.checkout_head(Some(CheckoutBuilder::new().force()))?;
            return Ok(SyncOutcome::FastForwarded);
        }

        // Diverged → real merge.
        self.repo.merge(&[&annotated], None, None)?;
        let mut index = self.repo.index()?;
        if index.has_conflicts() {
            // Leave conflict markers in the working tree and surface it via the error (the old
            // `.sync-conflict` flag file was write-only residue — nothing ever read it).
            bail!("merge conflict — conflict markers written, never dropped a highlight");
        }
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let sig = self.signature()?;
        let local = self.repo.head()?.peel_to_commit()?;
        let theirs = self.repo.find_commit(remote_oid)?;
        self.repo
            .commit(Some("HEAD"), &sig, &sig, "merge remote vault", &tree, &[&local, &theirs])?;
        self.repo.cleanup_state().ok();
        Ok(SyncOutcome::Merged)
    }

    /// Push `branch` to origin.
    pub fn push(&self, branch: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let mut po = PushOptions::new();
        po.remote_callbacks(callbacks(self.ssh_key.clone()));
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        remote.push(&[refspec.as_str()], Some(&mut po)).context("git push")?;
        Ok(())
    }

    /// True if local `branch` has commits not on `origin/<branch>` — so a push is still owed even
    /// when this run committed nothing (e.g. a previous push was interrupted, stranding the commit).
    /// Compared against the remote-tracking ref, which `pull`'s fetch refreshes each cycle.
    pub fn is_ahead(&self, branch: &str) -> Result<bool> {
        let local = self.repo.refname_to_id(&format!("refs/heads/{branch}"))?;
        match self.repo.refname_to_id(&format!("refs/remotes/origin/{branch}")) {
            Ok(remote) if remote == local => Ok(false),
            Ok(remote) => Ok(self.repo.graph_ahead_behind(local, remote)?.0 > 0),
            Err(_) => Ok(true), // no remote-tracking ref yet → a push is owed
        }
    }
}

/// One full sync cycle: commit local edits → pull → push.
pub fn sync_vault(
    url: &str,
    local: &Path,
    branch: Option<&str>,
    ssh_key: Option<PathBuf>,
    commit_message: &str,
) -> Result<SyncReport> {
    let vault = VaultRepo::open_or_clone(url, local, branch, ssh_key)?;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => vault.current_branch()?,
    };
    let committed = vault.commit_all(commit_message)?;
    let outcome = vault.pull(&branch)?;
    // Push when we committed, when a merge produced a new commit, OR when local is simply ahead of
    // origin — the last case self-heals a commit stranded by an earlier interrupted/failed push.
    let mut pushed = false;
    if committed.is_some() || outcome == SyncOutcome::Merged || vault.is_ahead(&branch)? {
        vault.push(&branch)?;
        pushed = true;
    }
    Ok(SyncReport { committed, outcome, pushed })
}

/// Git credentials: SSH key (device) → ssh-agent → default (local file:// remotes need none).
fn callbacks(ssh_key: Option<PathBuf>) -> RemoteCallbacks<'static> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(move |_url, username, allowed| {
        let user = username.unwrap_or("git");
        if allowed.contains(CredentialType::SSH_KEY) {
            if let Some(key) = &ssh_key {
                return Cred::ssh_key(user, None, key, None);
            }
            return Cred::ssh_key_from_agent(user);
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            return Cred::default();
        }
        Cred::default()
    });
    cb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(repo_dir: &Path, name: &str, body: &str) {
        std::fs::write(repo_dir.join(name), body).unwrap();
    }

    #[test]
    fn clone_commit_push_pull_round_trip() {
        let root = std::env::temp_dir().join(format!("sbsync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remote = root.join("remote.git");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&root).unwrap();

        // bare remote
        Repository::init_bare(&remote).unwrap();
        let url = format!("file://{}", remote.display());

        // clone A, add a note, commit, push
        let va = VaultRepo::open_or_clone(&url, &a, None, None).unwrap();
        write(&a, "note.md", "Hello ==world==\n");
        assert!(va.commit_all("add note").unwrap().is_some());
        let branch = va.current_branch().unwrap();
        va.push(&branch).unwrap();

        // committing again with no changes is a no-op
        assert!(va.commit_all("noop").unwrap().is_none());

        // clone B, should see the note
        let vb = VaultRepo::open_or_clone(&url, &b, Some(&branch), None).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("note.md")).unwrap(), "Hello ==world==\n");

        // A adds another file and pushes; B fast-forwards
        write(&a, "second.md", "more\n");
        va.commit_all("second").unwrap();
        va.push(&branch).unwrap();
        let outcome = vb.pull(&branch).unwrap();
        assert_eq!(outcome, SyncOutcome::FastForwarded);
        assert!(b.join("second.md").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_recovers_a_commit_stranded_by_an_earlier_failed_push() {
        let root = std::env::temp_dir().join(format!("sbsync-strand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remote = root.join("remote.git");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&root).unwrap();
        Repository::init_bare(&remote).unwrap();
        let url = format!("file://{}", remote.display());

        let va = VaultRepo::open_or_clone(&url, &a, None, None).unwrap();
        write(&a, "seed.md", "seed\n");
        va.commit_all("seed").unwrap();
        let branch = va.current_branch().unwrap();
        va.push(&branch).unwrap();

        // Simulate a highlight write whose push failed: commit locally, do NOT push.
        write(&a, "note.md", "device ==highlight==\n");
        assert!(va.commit_all("device highlight").unwrap().is_some());
        assert!(va.is_ahead(&branch).unwrap(), "ahead after an unpushed commit");

        // A later sync has nothing new to commit, but must still push the stranded commit.
        let report = sync_vault(&url, &a, Some(&branch), None, "resync").unwrap();
        assert!(report.committed.is_none(), "nothing new to commit");
        assert!(report.pushed, "the stranded commit is recovered and pushed");

        // A fresh clone now sees the recovered highlight.
        VaultRepo::open_or_clone(&url, &b, Some(&branch), None).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("note.md")).unwrap(), "device ==highlight==\n");

        // And a subsequent sync doesn't redundantly push (fetch refreshed the remote-tracking ref).
        let report2 = sync_vault(&url, &a, Some(&branch), None, "resync2").unwrap();
        assert!(!report2.pushed, "in sync now — no redundant push");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn independent_changes_merge_without_conflict() {
        let root = std::env::temp_dir().join(format!("sbsync2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remote = root.join("remote.git");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&root).unwrap();
        Repository::init_bare(&remote).unwrap();
        let url = format!("file://{}", remote.display());

        let va = VaultRepo::open_or_clone(&url, &a, None, None).unwrap();
        write(&a, "seed.md", "seed\n");
        va.commit_all("seed").unwrap();
        let branch = va.current_branch().unwrap();
        va.push(&branch).unwrap();

        let vb = VaultRepo::open_or_clone(&url, &b, Some(&branch), None).unwrap();

        // A and B edit DIFFERENT files, both commit; B pulls → merge commit, no conflict.
        write(&a, "from_a.md", "==a==\n");
        va.commit_all("a edit").unwrap();
        va.push(&branch).unwrap();

        write(&b, "from_b.md", "==b==\n");
        vb.commit_all("b edit").unwrap();
        let outcome = vb.pull(&branch).unwrap();
        assert_eq!(outcome, SyncOutcome::Merged);
        assert!(b.join("from_a.md").exists() && b.join("from_b.md").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
