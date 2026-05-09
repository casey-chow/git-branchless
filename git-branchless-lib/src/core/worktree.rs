//! Utilities for discovering and describing linked worktrees.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eyre::Context;

use crate::git::{GitRunInfo, GitRunOpts, NonZeroOid, ReferenceName, Repo};

/// Information about a linked worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Canonicalized worktree path, if available.
    pub path: PathBuf,

    /// Human-friendly name for the worktree, disambiguated within the current snapshot.
    pub display_name: String,

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
    /// Get a stable display name for the worktree.
    pub fn display_name(&self) -> String {
        self.display_name.clone()
    }
}

/// A snapshot of all linked worktrees for the current repository.
#[derive(Clone, Debug, Default)]
pub struct WorktreeSnapshot {
    /// All linked worktrees.
    pub entries: Vec<WorktreeEntry>,
}

impl WorktreeSnapshot {
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

    /// Return all active head commits across linked worktrees.
    pub fn active_head_oids(&self) -> HashSet<NonZeroOid> {
        self.entries
            .iter()
            .filter_map(|entry| entry.head_oid)
            .collect()
    }
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn get_path_display_name_candidates(path: &Path) -> Vec<String> {
    let segments: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        return vec![path.to_string_lossy().into_owned()];
    }
    (0..segments.len())
        .rev()
        .map(|start| segments[start..].join("/"))
        .collect()
}

fn update_display_names(entries: &mut [WorktreeEntry]) {
    let candidate_lists: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| get_path_display_name_candidates(&entry.path))
        .collect();

    for (index, entry) in entries.iter_mut().enumerate() {
        let display_name = candidate_lists[index]
            .iter()
            .find(|candidate| {
                candidate_lists
                    .iter()
                    .enumerate()
                    .all(|(other_index, other_candidates)| {
                        other_index == index
                            || !other_candidates.iter().any(|other| other == *candidate)
                    })
            })
            .cloned()
            .or_else(|| candidate_lists[index].last().cloned())
            .unwrap_or_else(|| entry.path.to_string_lossy().into_owned());
        entry.display_name = display_name;
    }
}

fn parse_worktree_head_info(
    lines: &[String],
) -> eyre::Result<(Option<NonZeroOid>, Option<ReferenceName>, bool)> {
    let mut head_oid = None;
    let mut branch_name = None;
    let mut detached = false;
    let mut prunable = false;

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
        } else if line.starts_with("prunable ") {
            prunable = true;
        }
    }

    if detached {
        branch_name = None;
    }

    Ok((head_oid, branch_name, prunable))
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
        let (head_oid, branch_name, prunable) = parse_worktree_head_info(current_lines)?;
        if prunable {
            current_lines.clear();
            return Ok(());
        }
        let path = canonicalize_best_effort(&path);
        let is_current = current_worktree_path.as_ref() == Some(&path);
        let is_main = main_worktree_path.as_ref() == Some(&path);
        entries.push(WorktreeEntry {
            path,
            display_name: String::new(),
            head_oid,
            branch_name,
            is_current,
            is_main,
        });
        current_lines.clear();
        Ok(())
    };

    for field in stdout.split('\0') {
        if field.is_empty() {
            flush(&mut current_path, &mut current_lines, &mut entries)?;
            continue;
        }

        if let Some(path) = field.strip_prefix("worktree ") {
            flush(&mut current_path, &mut current_lines, &mut entries)?;
            current_path = Some(PathBuf::from(path));
        } else {
            current_lines.push(field.to_owned());
        }
    }
    flush(&mut current_path, &mut current_lines, &mut entries)?;
    update_display_names(&mut entries);

    Ok(WorktreeSnapshot { entries })
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
        &["worktree", "list", "--porcelain", "-z"],
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
