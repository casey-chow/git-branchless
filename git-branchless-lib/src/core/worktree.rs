//! Utilities for discovering and describing linked worktrees.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::Context;

use crate::git::{GitRunInfo, GitRunOpts, NonZeroOid, ReferenceName, Repo, ResolvedReferenceInfo};

/// Information about a linked worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Canonicalized worktree path, if available.
    pub path: PathBuf,

    /// The OID checked out in the worktree.
    pub head_oid: Option<NonZeroOid>,

    /// The checked-out branch reference, if any.
    pub branch_name: Option<ReferenceName>,

    /// Whether this is the current worktree.
    pub is_current: bool,

    /// Whether this is the main/home worktree for the repository.
    pub is_main: bool,
}

impl WorktreeEntry {
    /// Returns `true` if the worktree is detached.
    pub fn is_detached(&self) -> bool {
        self.branch_name.is_none()
    }

    /// Get a stable display name for the worktree.
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| self.path.to_str().unwrap_or("<worktree>"))
            .to_owned()
    }
}

/// A snapshot of all linked worktrees for the current repository.
#[derive(Clone, Debug, Default)]
pub struct WorktreeSnapshot {
    /// All linked worktrees.
    pub entries: Vec<WorktreeEntry>,
}

impl WorktreeSnapshot {
    /// Build a map from commit OID to worktrees which currently have it checked out.
    pub fn oid_to_worktrees(&self) -> HashMap<NonZeroOid, Vec<&WorktreeEntry>> {
        let mut result: HashMap<NonZeroOid, Vec<&WorktreeEntry>> = HashMap::new();
        for entry in &self.entries {
            if let Some(oid) = entry.head_oid {
                result.entry(oid).or_default().push(entry);
            }
        }
        result
    }

    /// Build a map from branch reference name to the owning checked-out worktree.
    pub fn branch_to_worktree(&self) -> HashMap<ReferenceName, &WorktreeEntry> {
        self.entries
            .iter()
            .filter_map(|entry| {
                entry
                    .branch_name
                    .as_ref()
                    .map(|branch_name| (branch_name.clone(), entry))
            })
            .collect()
    }

    /// Return the current worktree, if it could be identified.
    pub fn current(&self) -> Option<&WorktreeEntry> {
        self.entries.iter().find(|entry| entry.is_current)
    }

    /// Return the worktree which owns the provided branch.
    pub fn find_by_branch(&self, branch_name: &ReferenceName) -> Option<&WorktreeEntry> {
        self.entries
            .iter()
            .find(|entry| entry.branch_name.as_ref() == Some(branch_name))
    }

    /// Return all worktrees checked out at the provided OID.
    pub fn find_by_head_oid(&self, oid: NonZeroOid) -> Vec<&WorktreeEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.head_oid == Some(oid))
            .collect()
    }

    /// Return all active branch names.
    pub fn active_branch_names(&self) -> HashSet<ReferenceName> {
        self.entries
            .iter()
            .filter_map(|entry| entry.branch_name.clone())
            .collect()
    }

    /// Return all active detached head commits.
    pub fn active_detached_head_oids(&self) -> HashSet<NonZeroOid> {
        self.entries
            .iter()
            .filter(|entry| entry.is_detached())
            .filter_map(|entry| entry.head_oid)
            .collect()
    }
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn parse_worktree_head_info(
    lines: &[String],
) -> eyre::Result<(Option<NonZeroOid>, Option<ReferenceName>)> {
    let mut head_oid = None;
    let mut branch_name = None;
    let mut detached = false;

    for line in lines {
        if let Some(value) = line.strip_prefix("HEAD ") {
            head_oid = match value.parse() {
                Ok(oid) => Some(oid),
                Err(_) if value == "0000000000000000000000000000000000000000" => None,
                Err(err) => return Err(err).wrap_err("Parsing worktree HEAD OID"),
            };
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch_name = Some(ReferenceName::from(value));
        } else if line == "detached" {
            detached = true;
        }
    }

    if detached {
        branch_name = None;
    }

    Ok((head_oid, branch_name))
}

fn parse_worktree_snapshot(
    stdout: &str,
    current_worktree_path: Option<PathBuf>,
    main_worktree_path: Option<PathBuf>,
) -> eyre::Result<WorktreeSnapshot> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_lines: Vec<String> = Vec::new();

    let flush = |current_path: &mut Option<PathBuf>,
                 current_lines: &mut Vec<String>,
                 entries: &mut Vec<WorktreeEntry>|
     -> eyre::Result<()> {
        let Some(path) = current_path.take() else {
            current_lines.clear();
            return Ok(());
        };
        let path = canonicalize_best_effort(&path);
        let (head_oid, branch_name) = parse_worktree_head_info(current_lines)?;
        let is_current = current_worktree_path.as_ref() == Some(&path);
        let is_main = main_worktree_path.as_ref() == Some(&path);
        entries.push(WorktreeEntry {
            path,
            head_oid,
            branch_name,
            is_current,
            is_main,
        });
        current_lines.clear();
        Ok(())
    };

    for line in stdout.lines() {
        if line.is_empty() {
            flush(&mut current_path, &mut current_lines, &mut entries)?;
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            flush(&mut current_path, &mut current_lines, &mut entries)?;
            current_path = Some(PathBuf::from(path));
        } else {
            current_lines.push(line.to_owned());
        }
    }
    flush(&mut current_path, &mut current_lines, &mut entries)?;

    Ok(WorktreeSnapshot { entries })
}

/// Discover all linked worktrees for the current repository using the ambient `git` command.
pub fn get_linked_worktrees_for_repo(repo: &Repo) -> eyre::Result<WorktreeSnapshot> {
    let current_worktree_path = repo
        .get_working_copy_path()
        .map(|path| canonicalize_best_effort(&path));
    let main_worktree_path = repo
        .open_worktree_parent_repo()?
        .as_ref()
        .unwrap_or(repo)
        .get_working_copy_path()
        .map(|path| canonicalize_best_effort(&path));
    let working_directory = repo
        .get_working_copy_path()
        .unwrap_or_else(|| repo.get_path().to_path_buf());
    let output = Command::new("git")
        .current_dir(working_directory)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .wrap_err("Running `git worktree list --porcelain`")?;
    if !output.status.success() {
        return Ok(WorktreeSnapshot::default());
    }

    let stdout = String::from_utf8(output.stdout).wrap_err("Decoding worktree list")?;
    parse_worktree_snapshot(&stdout, current_worktree_path, main_worktree_path)
}

/// Discover all linked worktrees for the current repository.
pub fn get_linked_worktrees(
    git_run_info: &GitRunInfo,
    repo: &Repo,
) -> eyre::Result<WorktreeSnapshot> {
    let current_worktree_path = repo
        .get_working_copy_path()
        .map(|path| canonicalize_best_effort(&path));
    let main_worktree_path = repo
        .open_worktree_parent_repo()?
        .as_ref()
        .unwrap_or(repo)
        .get_working_copy_path()
        .map(|path| canonicalize_best_effort(&path));
    let result = git_run_info.run_silent(
        repo,
        None,
        &["worktree", "list", "--porcelain"],
        GitRunOpts {
            treat_git_failure_as_error: false,
            ..Default::default()
        },
    )?;
    if !result.exit_code.is_success() {
        return Ok(WorktreeSnapshot::default());
    }

    let stdout = String::from_utf8(result.stdout).wrap_err("Decoding worktree list")?;
    parse_worktree_snapshot(&stdout, current_worktree_path, main_worktree_path)
}

/// Find the current worktree-relative HEAD information among linked worktrees.
pub fn make_current_worktree_head(
    snapshot: &WorktreeSnapshot,
    head_info: &ResolvedReferenceInfo,
) -> Option<WorktreeEntry> {
    snapshot.current().cloned().or_else(|| {
        Some(WorktreeEntry {
            path: PathBuf::new(),
            head_oid: head_info.oid,
            branch_name: head_info.reference_name.clone(),
            is_current: true,
            is_main: false,
        })
    })
}
