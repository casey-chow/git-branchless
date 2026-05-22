//! Manage linked worktrees with branchless-friendly defaults.

use std::ffi::OsString;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use cursive_core::theme::BaseColor;
use cursive_core::utils::markup::StyledString;
use eyre::WrapErr;
use lib::core::dag::{Dag, union_all};
use lib::core::effects::Effects;
use lib::core::eventlog::{EventLogDb, EventReplayer};
use lib::core::node_descriptors::RelativeTimeDescriptor;
use lib::core::repo_ext::RepoExt;
use lib::core::worktree::{WorktreeEntry, WorktreeSnapshot, get_linked_worktrees};
use lib::git::{
    BranchType, ConfigRead, GitErrorCode, GitRunInfo, NonZeroOid, ReferenceName, Repo, RepoError,
};
use lib::try_exit_code;
use lib::util::{ExitCode, EyreExitOr, get_sh};
use tracing::instrument;

use git_branchless_opts::{ResolveRevsetOptions, Revset, WorktreeArgs, WorktreeSubcommand};
use git_branchless_revset::resolve_commits;

fn expand_home(path: PathBuf) -> eyre::Result<PathBuf> {
    let path_string = path.to_string_lossy();
    if path_string == "~" || path_string.starts_with("~/") {
        let home_dir = std::env::var_os("HOME").ok_or_else(|| eyre::eyre!("$HOME is not set"))?;
        let suffix = path_string.strip_prefix('~').unwrap();
        Ok(PathBuf::from(home_dir).join(suffix.trim_start_matches('/')))
    } else {
        Ok(path)
    }
}

fn repo_display_name(repo: &Repo) -> eyre::Result<String> {
    let parent_repo = repo.open_worktree_parent_repo()?;
    let repo = parent_repo.as_ref().unwrap_or(repo);
    let worktree_path = repo
        .get_working_copy_path()
        .ok_or_else(|| eyre::eyre!("Repository does not have a working copy path"))?;
    Ok(worktree_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_owned())
}

fn sanitize_worktree_name(name: &str) -> eyre::Result<String> {
    let mut sanitized = String::with_capacity(name.len());
    let mut previous_was_separator = false;
    for ch in name.chars() {
        let replacement = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            previous_was_separator = false;
            ch
        } else {
            if previous_was_separator {
                continue;
            }
            previous_was_separator = true;
            '-'
        };
        sanitized.push(replacement);
    }
    let sanitized = sanitized.trim_matches(['.', ' ', '-']).to_owned();
    if sanitized.is_empty() {
        eyre::bail!("Worktree name '{name}' is not usable after sanitization")
    } else {
        Ok(sanitized)
    }
}

fn get_worktree_root(repo: &Repo) -> eyre::Result<PathBuf> {
    let config = repo.get_readonly_config()?;
    if let Some(path) = config.get::<PathBuf, _>("branchless.worktree.root")? {
        return expand_home(path);
    }

    let home_dir = std::env::var_os("HOME").ok_or_else(|| eyre::eyre!("$HOME is not set"))?;
    Ok(PathBuf::from(home_dir).join(".worktrees"))
}

fn get_worktree_path(repo: &Repo, requested_name: &str) -> eyre::Result<PathBuf> {
    let root = get_worktree_root(repo)?;
    let repo_name = repo_display_name(repo)?;
    let sanitized_name = sanitize_worktree_name(requested_name)?;
    let base = root.join(repo_name);
    let candidate = base.join(&sanitized_name);
    if !candidate.exists() {
        return Ok(candidate);
    }

    for index in 2.. {
        let candidate = base.join(format!("{sanitized_name}-{index}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    unreachable!("Exhausted numeric suffixes for worktree names")
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_target_oid(effects: &Effects, repo: &Repo, target: &Revset) -> eyre::Result<NonZeroOid> {
    let conn = repo.get_db_conn()?;
    let event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, repo, &event_log_db)?;
    let event_cursor = event_replayer.make_default_cursor();
    let references_snapshot = repo.get_references_snapshot()?;
    let mut dag = Dag::open_and_sync(
        effects,
        repo,
        &event_replayer,
        event_cursor,
        &references_snapshot,
    )?;
    let commit_sets = resolve_commits(
        effects,
        repo,
        &mut dag,
        std::slice::from_ref(target),
        &ResolveRevsetOptions::default(),
    )?;
    let commit_set = dag.query_heads(union_all(&commit_sets))?;
    let commit_oids = dag.commit_set_to_vec(&commit_set)?;
    match commit_oids.as_slice() {
        [oid] => Ok(*oid),
        [] => eyre::bail!("Target '{target}' did not resolve to any commits"),
        _ => eyre::bail!("Target '{target}' resolved to multiple heads"),
    }
}

fn find_local_branch(repo: &Repo, target: &str) -> eyre::Result<Option<ReferenceName>> {
    match repo.find_branch(target, BranchType::Local) {
        Ok(Some(branch)) => Ok(Some(branch.get_reference_name()?)),
        Ok(None) => Ok(None),
        Err(RepoError::FindBranch { source, .. }) if source.code() == GitErrorCode::InvalidSpec => {
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

fn print_worktree_path(effects: &Effects, action: &str, entry: &WorktreeEntry) -> eyre::Result<()> {
    writeln!(
        effects.get_output_stream(),
        "{} {}",
        action,
        entry.path.to_string_lossy()
    )?;
    Ok(())
}

fn get_post_create_hook(repo: &Repo) -> eyre::Result<Option<String>> {
    repo.get_readonly_config()?
        .get("branchless.worktree.postCreateHook")
}

fn run_post_create_hook(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    entry: &WorktreeEntry,
) -> EyreExitOr<()> {
    let Some(hook_command) = get_post_create_hook(repo)? else {
        return Ok(Ok(()));
    };

    let shell = get_sh().ok_or_else(|| eyre::eyre!("could not get sh"))?;
    let mut command = Command::new(shell);
    command.current_dir(&entry.path);
    command.arg("-c").arg(&hook_command);
    command.env_clear();
    command.envs(git_run_info.env.iter());
    command.env("BRANCHLESS_WORKTREE_PATH", &entry.path);
    command.env("BRANCHLESS_WORKTREE_NAME", entry.display_name());
    if let Some(branch_name) = &entry.branch_name {
        command.env(
            "BRANCHLESS_WORKTREE_BRANCH",
            branch_name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_name.as_str()),
        );
    }
    if let Some(head_oid) = entry.head_oid {
        command.env("BRANCHLESS_WORKTREE_HEAD", head_oid.to_string());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let output = command
        .output()
        .wrap_err("Running worktree post-create hook")?;
    write!(
        effects.get_output_stream(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    )
    .wrap_err("Writing post-create hook stdout")?;
    write!(
        effects.get_error_stream(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    )
    .wrap_err("Writing post-create hook stderr")?;

    let exit_code = ExitCode(output.status.code().unwrap_or(1).try_into()?);
    if exit_code.is_success() {
        Ok(Ok(()))
    } else {
        writeln!(
            effects.get_error_stream(),
            "Worktree post-create hook failed for '{}'.",
            entry.display_name()
        )?;
        Ok(Err(exit_code))
    }
}

fn run_git_worktree_command(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    args: &[OsString],
) -> EyreExitOr<()> {
    let args: Vec<&std::ffi::OsStr> = args.iter().map(OsString::as_os_str).collect();
    git_run_info.run(effects, None, &args)
}

fn add_worktree(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    requested_name: &str,
    target: Option<&Revset>,
    new_branch: Option<&str>,
) -> EyreExitOr<()> {
    let target_text = target.map(ToString::to_string);
    let target_branch_name = match target_text.as_deref() {
        Some(target_text) => find_local_branch(repo, target_text)?,
        None => None,
    };
    let git_target = match target {
        Some(_target) if target_branch_name.is_some() => target_text.clone(),
        Some(target) => Some(resolve_target_oid(effects, repo, target)?.to_string()),
        None => None,
    };

    let worktree_snapshot = get_linked_worktrees(git_run_info, repo)?;
    let maybe_branch_name = match (new_branch, target_branch_name) {
        (Some(branch_name), _) => Some(ReferenceName::from(format!("refs/heads/{branch_name}"))),
        (None, branch_name) => branch_name,
    };

    if let Some(branch_name) = maybe_branch_name.as_ref() {
        if worktree_snapshot.find_by_branch(branch_name).is_some() {
            let branch_name = branch_name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_name.as_str());
            writeln!(
                effects.get_error_stream(),
                "Branch '{branch_name}' is already active in another worktree."
            )?;
            writeln!(
                effects.get_error_stream(),
                "Use `git wt list` to find that worktree."
            )?;
            return Ok(Err(ExitCode(1)));
        }
    }

    let worktree_path = get_worktree_path(repo, requested_name)?;
    let mut args = vec![OsString::from("worktree"), OsString::from("add")];
    if let Some(branch_name) = new_branch {
        args.push(OsString::from("-b"));
        args.push(OsString::from(branch_name));
    } else if maybe_branch_name.is_none() {
        args.push(OsString::from("--detach"));
    }
    args.push(worktree_path.as_os_str().to_os_string());
    if let Some(git_target) = git_target {
        args.push(OsString::from(git_target));
    }
    match run_git_worktree_command(effects, git_run_info, &args)? {
        Ok(()) => {}
        Err(exit_code) => return Ok(Err(exit_code)),
    }

    let updated_snapshot = get_linked_worktrees(git_run_info, repo)?;
    let worktree_path = canonicalize_best_effort(&worktree_path);
    let entry = updated_snapshot
        .entries
        .into_iter()
        .find(|entry| entry.path == worktree_path)
        .ok_or_else(|| eyre::eyre!("Created worktree was not discoverable afterwards"))?;
    try_exit_code!(run_post_create_hook(effects, git_run_info, repo, &entry)?);
    print_worktree_path(effects, "Created worktree at:", &entry)?;
    Ok(Ok(()))
}

fn describe_worktree_summary(repo: &Repo, entry: &WorktreeEntry) -> eyre::Result<StyledString> {
    let mut summary = StyledString::new();
    let worktree_icon = if entry.is_current { "ᐅ" } else { "⎇" };
    summary.append_styled(worktree_icon, BaseColor::Blue.light());
    summary.append_plain(" ");
    summary.append_styled(entry.display_name(), BaseColor::Blue.light());

    if let Some(branch_name) = &entry.branch_name {
        let branch_name = branch_name
            .as_str()
            .strip_prefix("refs/heads/")
            .unwrap_or(branch_name.as_str());
        summary.append_plain(" ");
        summary.append_styled(branch_name, BaseColor::Green.light());
    }

    match entry
        .head_oid
        .and_then(|oid| repo.find_commit(oid).ok().flatten())
    {
        Some(commit) => {
            summary.append_plain(" ");
            summary.append_styled(commit.get_short_oid()?, BaseColor::Yellow.dark());
            summary.append_plain(" ");
            summary.append_styled(
                RelativeTimeDescriptor::describe_time_delta(
                    SystemTime::now(),
                    commit.get_time().to_system_time()?,
                )?,
                BaseColor::Green.dark(),
            );
            summary.append_plain(" ");
            summary.append_plain(commit.get_summary()?.to_string());
            Ok(summary)
        }
        None => {
            if let Some(oid) = entry.head_oid {
                summary.append_plain(" ");
                summary.append_styled(&oid.to_string()[..8], BaseColor::Yellow.dark());
            }
            summary.append_plain(" ");
            summary.append_styled("<unborn or unavailable>", BaseColor::Yellow.light());
            Ok(summary)
        }
    }
}

fn resolve_rm_target(
    repo: &Repo,
    snapshot: &WorktreeSnapshot,
    target: Option<&str>,
) -> eyre::Result<WorktreeEntry> {
    if let Some(target) = target {
        let target_path = canonicalize_best_effort(Path::new(target));
        if let Some(entry) = snapshot.entries.iter().find(|entry| {
            entry.path == target_path
                || entry.display_name() == target
                || entry.path.to_string_lossy() == target
        }) {
            return Ok(entry.clone());
        }
        if let Some(branch_name) = find_local_branch(repo, target)? {
            if let Some(entry) = snapshot.find_by_branch(&branch_name) {
                return Ok(entry.clone());
            }
        }
        eyre::bail!("Could not resolve worktree target '{target}'")
    } else {
        let current = snapshot
            .current()
            .ok_or_else(|| eyre::eyre!("Could not determine the current worktree"))?;
        Ok(current.clone())
    }
}

fn remove_worktree(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    target: Option<&str>,
    force: bool,
) -> EyreExitOr<()> {
    let snapshot = get_linked_worktrees(git_run_info, repo)?;
    let entry = resolve_rm_target(repo, &snapshot, target)?;
    if entry.is_main && target.is_none() {
        writeln!(
            effects.get_error_stream(),
            "Refusing to remove the main worktree implicitly."
        )?;
        return Ok(Err(ExitCode(1)));
    }
    if entry.is_current {
        writeln!(
            effects.get_error_stream(),
            "Refusing to remove the current worktree from inside it."
        )?;
        writeln!(
            effects.get_error_stream(),
            "Run `git wt rm {}` from another worktree instead.",
            entry.display_name()
        )?;
        return Ok(Err(ExitCode(1)));
    }

    let parent_repo = repo.open_worktree_parent_repo()?;
    let repo = parent_repo.as_ref().unwrap_or(repo);
    let parent_working_directory = repo
        .get_working_copy_path()
        .ok_or_else(|| eyre::eyre!("Repository does not have a working copy path"))?;
    let git_run_info = GitRunInfo {
        working_directory: parent_working_directory,
        ..git_run_info.clone()
    };
    let args = vec![OsString::from("worktree"), OsString::from("remove")];
    let mut args = args;
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(entry.path.as_os_str().to_os_string());
    match run_git_worktree_command(effects, &git_run_info, &args)? {
        Ok(()) => {}
        Err(exit_code) => return Ok(Err(exit_code)),
    }
    writeln!(
        effects.get_output_stream(),
        "Removed worktree {}",
        entry.path.to_string_lossy()
    )?;
    Ok(Ok(()))
}

fn list_worktrees(effects: &Effects, git_run_info: &GitRunInfo) -> EyreExitOr<()> {
    let repo = Repo::from_dir(&git_run_info.working_directory)?;
    let snapshot = get_linked_worktrees(git_run_info, &repo)?;
    for (index, entry) in snapshot.entries.iter().enumerate() {
        let summary = describe_worktree_summary(&repo, entry)?;
        writeln!(
            effects.get_output_stream(),
            "{}",
            effects.get_glyphs().render(summary)?
        )?;
        let mut path = StyledString::new();
        path.append_styled(entry.path.to_string_lossy(), BaseColor::Black.light());
        writeln!(
            effects.get_output_stream(),
            "  {}",
            effects.get_glyphs().render(path)?
        )?;
        if index + 1 < snapshot.entries.len() {
            writeln!(effects.get_output_stream())?;
        }
    }
    Ok(Ok(()))
}

#[instrument]
pub fn command_main(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    args: WorktreeArgs,
) -> EyreExitOr<()> {
    let repo = Repo::from_current_dir()?;
    match args.subcommand {
        WorktreeSubcommand::Add {
            new_branch,
            name,
            target,
        } => add_worktree(
            effects,
            git_run_info,
            &repo,
            &name,
            target.as_ref(),
            new_branch.as_deref(),
        ),
        WorktreeSubcommand::Rm { force, target } => {
            remove_worktree(effects, git_run_info, &repo, target.as_deref(), force)
        }
        WorktreeSubcommand::List => list_worktrees(effects, git_run_info),
    }
}
