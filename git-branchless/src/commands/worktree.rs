//! Manage linked worktrees with branchless-friendly defaults.

use std::ffi::OsString;
use std::fmt::Write;
use std::fs::OpenOptions;
#[cfg(not(unix))]
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cursive_core::theme::BaseColor;
use cursive_core::utils::markup::StyledString;
use eyre::WrapErr;
#[cfg(not(unix))]
use itertools::Itertools;
use lib::core::effects::Effects;
use lib::core::formatting::Glyphs;
use lib::core::node_descriptors::RelativeTimeDescriptor;
use lib::core::worktree::{WorktreeEntry, WorktreeSnapshot, get_linked_worktrees};
use lib::git::{
    BranchType, ConfigRead, GitErrorCode, GitRunInfo, NonZeroOid, ReferenceName, Repo, RepoError,
};
use lib::util::{ExitCode, EyreExitOr, get_sh};
use tracing::instrument;

use git_branchless_init::SHELL_DIRECTIVE_FILE_ENV_VAR;
use git_branchless_opts::{ResolveRevsetOptions, Revset, WorktreeArgs, WorktreeSubcommand};
use git_branchless_revset::resolve_commits;
use git_branchless_smartlog::{SmartlogOptions, smartlog};
use lib::core::dag::{Dag, union_all};
use lib::core::eventlog::{EventLogDb, EventReplayer};
use lib::core::repo_ext::RepoExt;
use lib::try_exit_code;

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

fn get_cd_command(path: &Path) -> String {
    let path = path.to_string_lossy();
    let quoted_path = format!("'{}'", path.replace('\'', "'\"'\"'"));
    format!("cd {quoted_path}")
}

fn get_shell_directive_path() -> Option<PathBuf> {
    std::env::var_os(SHELL_DIRECTIVE_FILE_ENV_VAR).map(PathBuf::from)
}

fn write_shell_cd(path: &Path) -> eyre::Result<()> {
    let directive_path = get_shell_directive_path()
        .ok_or_else(|| eyre::eyre!("{SHELL_DIRECTIVE_FILE_ENV_VAR} is not set"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&directive_path)
        .wrap_err_with(|| format!("Opening shell directive file {:?}", directive_path))?;
    std::io::Write::write_all(&mut file, format!("{}\n", get_cd_command(path)).as_bytes())
        .wrap_err_with(|| format!("Writing shell directive file {:?}", directive_path))?;
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

fn get_worktree_smartlog_preview(
    git_run_info: &GitRunInfo,
    entry: &WorktreeEntry,
) -> eyre::Result<String> {
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let effects = Effects::new_from_buffer_for_test(Glyphs::pretty(), &stdout, &stderr);
    let git_run_info = GitRunInfo {
        working_directory: entry.path.clone(),
        ..git_run_info.clone()
    };
    match smartlog(
        &effects,
        &git_run_info,
        SmartlogOptions {
            event_id: None,
            revset: None,
            resolve_revset_options: ResolveRevsetOptions::default(),
            reverse: false,
            exact: false,
            include_related_commits: true,
        },
    )? {
        Ok(()) => {
            let stdout = stdout.lock().unwrap();
            Ok(String::from_utf8_lossy(&stdout).trim_end().to_string())
        }
        Err(_exit_code) => {
            let stderr = stderr.lock().unwrap();
            let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
            if stderr.is_empty() {
                Ok("<unable to render smartlog preview>".to_string())
            } else {
                Ok(format!("<unable to render smartlog preview>\n{stderr}"))
            }
        }
    }
}

fn run_git_worktree_command(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    _repo: &Repo,
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
            writeln!(
                effects.get_error_stream(),
                "Branch '{}' is already active in another worktree.",
                branch_name
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_name.as_str())
            )?;
            writeln!(
                effects.get_error_stream(),
                "Use `git wt sw {}` to reuse that worktree.",
                branch_name
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_name.as_str())
            )?;
            return Ok(Err(ExitCode(1)));
        }
    }

    let worktree_path = get_worktree_path(repo, requested_name)?;
    let mut args = vec!["worktree".into(), "add".into()];
    if let Some(branch_name) = new_branch {
        args.push("-b".into());
        args.push(branch_name.into());
    } else if maybe_branch_name.is_none() {
        args.push("--detach".into());
    }
    args.push(worktree_path.as_os_str().to_os_string());
    if let Some(git_target) = git_target {
        args.push(git_target.into());
    }
    match run_git_worktree_command(effects, git_run_info, repo, &args)? {
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

#[cfg(not(unix))]
fn prompt_select_worktree(
    effects: &Effects,
    _git_run_info: &GitRunInfo,
    _repo: &Repo,
    snapshot: &WorktreeSnapshot,
    _initial_query: &str,
) -> eyre::Result<Option<WorktreeEntry>> {
    writeln!(effects.get_output_stream(), "Select a linked worktree:")?;
    for (index, entry) in snapshot.entries.iter().enumerate() {
        let branch = entry
            .branch_name
            .as_ref()
            .map(|branch| {
                branch
                    .as_str()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch.as_str())
            })
            .unwrap_or("detached");
        let head = entry
            .head_oid
            .map(|oid| oid.to_string()[..8].to_string())
            .unwrap_or_else(|| "--------".to_string());
        let current = if entry.is_current { "*" } else { " " };
        writeln!(
            effects.get_output_stream(),
            "  {}. {} {} {} {}",
            index + 1,
            current,
            branch,
            head,
            entry.display_name()
        )?;
    }

    loop {
        writeln!(
            effects.get_output_stream(),
            "Enter number or worktree name:"
        )?;
        let mut input = String::new();
        let bytes_read = io::stdin().read_line(&mut input)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(None);
        }
        if let Ok(index) = input.parse::<usize>() {
            if let Some(entry) = snapshot.entries.get(index.saturating_sub(1)) {
                return Ok(Some(entry.clone()));
            }
        }

        let matches = snapshot
            .entries
            .iter()
            .filter(|entry| {
                entry.display_name() == input
                    || entry.path.to_string_lossy() == input
                    || entry
                        .branch_name
                        .as_ref()
                        .map(|branch| {
                            branch
                                .as_str()
                                .strip_prefix("refs/heads/")
                                .unwrap_or(branch.as_str())
                                == input
                        })
                        .unwrap_or(false)
            })
            .cloned()
            .collect_vec();
        match matches.as_slice() {
            [entry] => return Ok(Some(entry.clone())),
            [] => {
                writeln!(
                    effects.get_error_stream(),
                    "No linked worktree matched '{input}'."
                )?;
            }
            [_, _, ..] => {
                writeln!(
                    effects.get_error_stream(),
                    "Input '{input}' matched multiple worktrees; enter a numeric index instead."
                )?;
            }
        }
    }
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

fn describe_worktree_entry(
    repo: &Repo,
    git_run_info: &GitRunInfo,
    entry: &WorktreeEntry,
) -> eyre::Result<(StyledString, String)> {
    let summary = describe_worktree_summary(repo, entry)?;
    let mut preview = String::new();
    writeln!(preview, "{}", entry.path.to_string_lossy())?;
    if let Some(branch_name) = &entry.branch_name {
        writeln!(
            preview,
            "branch: {}",
            branch_name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_name.as_str())
        )?;
    } else {
        writeln!(preview, "branch: detached")?;
    }
    if let Some(oid) = entry.head_oid {
        writeln!(preview, "head: {}", oid)?;
    }
    writeln!(preview)?;
    write!(
        preview,
        "{}",
        get_worktree_smartlog_preview(git_run_info, entry)?
    )?;
    Ok((summary, preview))
}

#[cfg(unix)]
fn prompt_select_worktree(
    _effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    snapshot: &WorktreeSnapshot,
    initial_query: &str,
) -> eyre::Result<Option<WorktreeEntry>> {
    worktree_skim::prompt(git_run_info, repo, snapshot, initial_query)
}

fn switch_worktree(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    interactive: bool,
    target: Option<&Revset>,
) -> EyreExitOr<()> {
    if get_shell_directive_path().is_none() {
        writeln!(
            effects.get_error_stream(),
            "`git wt sw` requires shell integration. Run `git branchless shell install` and use the installed command."
        )?;
        return Ok(Err(ExitCode(1)));
    }

    let snapshot = get_linked_worktrees(git_run_info, repo)?;
    let selected = if interactive {
        let initial_query = target.map(ToString::to_string).unwrap_or_default();
        match prompt_select_worktree(effects, git_run_info, repo, &snapshot, &initial_query)? {
            Some(entry) => entry,
            None => return Ok(Err(ExitCode(1))),
        }
    } else {
        let Some(target) = target else {
            writeln!(
                effects.get_error_stream(),
                "Provide a target or pass `-i/--interactive`."
            )?;
            return Ok(Err(ExitCode(1)));
        };
        let target_text = target.to_string();
        if let Some(entry) = snapshot.entries.iter().find(|entry| {
            entry.display_name() == target_text
                || entry.path.to_string_lossy() == target_text
                || entry.path == Path::new(&target_text)
        }) {
            entry.clone()
        } else if let Some(branch_name) = find_local_branch(repo, &target_text)? {
            match snapshot.find_by_branch(&branch_name) {
                Some(entry) => entry.clone(),
                None => {
                    writeln!(
                        effects.get_error_stream(),
                        "Branch '{target_text}' is not active in any linked worktree."
                    )?;
                    writeln!(
                        effects.get_error_stream(),
                        "Use `git wt add <name> {target_text}` instead."
                    )?;
                    return Ok(Err(ExitCode(1)));
                }
            }
        } else {
            let oid = match resolve_target_oid(effects, repo, target) {
                Ok(oid) => oid,
                Err(err) => {
                    writeln!(
                        effects.get_error_stream(),
                        "Could not resolve switch target '{target_text}': {err}"
                    )?;
                    return Ok(Err(ExitCode(1)));
                }
            };
            let matches = snapshot.find_by_head_oid(oid);
            match matches.as_slice() {
                [entry] => (*entry).clone(),
                [] => {
                    writeln!(
                        effects.get_error_stream(),
                        "Commit '{}' is not active in any linked worktree.",
                        &oid.to_string()[..8]
                    )?;
                    return Ok(Err(ExitCode(1)));
                }
                [_, _, ..] => {
                    writeln!(
                        effects.get_error_stream(),
                        "Commit '{}' is active in multiple worktrees; use `git wt sw -i` to choose one.",
                        &oid.to_string()[..8]
                    )?;
                    return Ok(Err(ExitCode(1)));
                }
            }
        }
    };

    write_shell_cd(&selected.path)?;
    Ok(Ok(()))
}

fn resolve_finish_target(
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

fn is_main_worktree(repo: &Repo, entry: &WorktreeEntry) -> eyre::Result<bool> {
    let _ = repo;
    Ok(entry.is_main)
}

fn finish_worktree(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    repo: &Repo,
    target: Option<&str>,
) -> EyreExitOr<()> {
    let snapshot = get_linked_worktrees(git_run_info, repo)?;
    let entry = resolve_finish_target(repo, &snapshot, target)?;
    if is_main_worktree(repo, &entry)? && target.is_none() {
        writeln!(
            effects.get_error_stream(),
            "Refusing to finish the main worktree implicitly."
        )?;
        return Ok(Err(ExitCode(1)));
    }

    let parent_repo = repo.open_worktree_parent_repo()?;
    let repo = parent_repo.as_ref().unwrap_or(repo);
    let parent_working_directory = repo
        .get_working_copy_path()
        .ok_or_else(|| eyre::eyre!("Repository does not have a working copy path"))?;
    let should_switch_to_main_worktree = canonicalize_best_effort(&git_run_info.working_directory)
        .starts_with(canonicalize_best_effort(&entry.path));
    if should_switch_to_main_worktree {
        if get_shell_directive_path().is_none() {
            writeln!(
                effects.get_error_stream(),
                "Refusing to finish the current worktree '{}'. Run it from the installed shell command so branchless can return you to the main worktree.",
                entry.display_name()
            )?;
            return Ok(Err(ExitCode(1)));
        }
    }
    let git_run_info = GitRunInfo {
        working_directory: parent_working_directory.clone(),
        ..git_run_info.clone()
    };
    let args = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        entry.path.as_os_str().to_os_string(),
    ];
    match run_git_worktree_command(effects, &git_run_info, repo, &args)? {
        Ok(()) => {}
        Err(exit_code) => return Ok(Err(exit_code)),
    }
    writeln!(
        effects.get_output_stream(),
        "Finished worktree {}",
        entry.path.to_string_lossy()
    )?;
    if should_switch_to_main_worktree {
        write_shell_cd(&parent_working_directory)?;
    }
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

#[cfg(unix)]
mod worktree_skim {
    use std::borrow::Cow;
    use std::sync::Arc;

    use eyre::eyre;
    use skim::{
        AnsiString, DisplayContext, ItemPreview, Matches, PreviewContext, Skim, SkimItem,
        SkimItemReceiver, SkimItemSender, prelude::SkimOptionsBuilder,
    };

    use super::*;

    #[derive(Clone, Debug)]
    struct WorktreeSkimItem {
        entry: WorktreeEntry,
        styled_summary: String,
        styled_preview: String,
    }

    impl SkimItem for WorktreeSkimItem {
        fn text(&self) -> Cow<'_, str> {
            AnsiString::parse(&self.styled_summary).into_inner()
        }

        fn display<'b>(&'b self, context: DisplayContext<'b>) -> AnsiString<'b> {
            let mut text = AnsiString::parse(&self.styled_summary);
            match context.matches {
                Matches::CharIndices(indices) => {
                    text.override_attrs(
                        indices
                            .iter()
                            .map(|&i| {
                                (
                                    context.highlight_attr,
                                    (u32::try_from(i).unwrap(), u32::try_from(i + 1).unwrap()),
                                )
                            })
                            .collect(),
                    );
                }
                Matches::CharRange(start, end) => {
                    text.override_attrs(vec![(
                        context.highlight_attr,
                        (u32::try_from(start).unwrap(), u32::try_from(end).unwrap()),
                    )]);
                }
                Matches::ByteRange(start, end) => {
                    let start = text.stripped()[..start].chars().count();
                    let end = start + text.stripped()[start..end].chars().count();
                    text.override_attrs(vec![(
                        context.highlight_attr,
                        (u32::try_from(start).unwrap(), u32::try_from(end).unwrap()),
                    )]);
                }
                Matches::None => (),
            }
            text
        }

        fn preview(&self, _context: PreviewContext) -> ItemPreview {
            ItemPreview::AnsiText(self.styled_preview.to_owned())
        }
    }

    pub fn prompt(
        git_run_info: &GitRunInfo,
        repo: &Repo,
        snapshot: &WorktreeSnapshot,
        initial_query: &str,
    ) -> eyre::Result<Option<WorktreeEntry>> {
        let options = SkimOptionsBuilder::default()
            .height("100%".to_string())
            .preview(Some("".to_string()))
            .preview_window("up:70%".to_string())
            .sync(true)
            .bind(vec!["Enter:accept".to_string()])
            .header(Some("Select a worktree".to_string()))
            .query(Some(initial_query.to_string()))
            .build()
            .map_err(|e| eyre!("building Skim options failed: {}", e))?;

        let items: Vec<WorktreeSkimItem> = snapshot
            .entries
            .iter()
            .cloned()
            .map(|entry| {
                let (summary, preview) = describe_worktree_entry(repo, git_run_info, &entry)?;
                Ok(WorktreeSkimItem {
                    entry,
                    styled_summary: Glyphs::pretty().render(summary)?,
                    styled_preview: preview,
                })
            })
            .collect::<eyre::Result<_>>()?;

        let rx_item = {
            let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = skim::prelude::unbounded();
            for item in items {
                tx_item.send(Arc::new(item))?;
            }
            rx_item
        };

        match Skim::run_with(&options, Some(rx_item)) {
            Some(result) => {
                if result.is_abort {
                    return Ok(None);
                }
                let selected = result
                    .selected_items
                    .first()
                    .and_then(|item| (*item).as_any().downcast_ref::<WorktreeSkimItem>());
                Ok(selected.map(|item| item.entry.clone()))
            }
            None => Ok(None),
        }
    }
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
        WorktreeSubcommand::Finish { target } => {
            finish_worktree(effects, git_run_info, &repo, target.as_deref())
        }
        WorktreeSubcommand::List => list_worktrees(effects, git_run_info),
        WorktreeSubcommand::Switch {
            interactive,
            target,
        } => switch_worktree(effects, git_run_info, &repo, interactive, target.as_ref()),
    }
}
