use lib::testing::pty::{run_in_pty, PtyAction};
use lib::testing::{make_git, make_git_worktree, GitRunOptions};
use std::process::Command;

const CARRIAGE_RETURN: &str = "\r";

fn set_worktree_root(git: &lib::testing::Git) -> eyre::Result<String> {
    let worktree_root = git.repo_path.join("test-worktrees");
    let worktree_root = worktree_root.to_string_lossy().to_string();
    git.run(&["config", "branchless.worktree.root", &worktree_root])?;
    Ok(worktree_root)
}

#[test]
fn test_init_installs_wt_alias() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;

    let (stdout, _stderr) = git.run(&["config", "alias.wt"])?;
    assert_eq!(stdout.trim(), "branchless worktree");

    Ok(())
}

#[test]
fn test_worktree_add_marks_detached_worktree_in_smartlog() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.detach_head()?;
    let test1_oid = git.commit_file("test1", 1)?;
    git.run(&["checkout", "master"])?;

    git.branchless("worktree", &["add", "side", &test1_oid.to_string()])?;

    let stdout = git.smartlog()?;
    assert!(stdout.contains("(⎇ side)"), "smartlog output was: {stdout}");
    assert!(stdout.contains(&test1_oid.to_string()[..7]));

    Ok(())
}

#[test]
fn test_worktree_add_resolves_revset_target_before_calling_git() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    let master_oid = git.get_repo()?.get_head_info()?.oid.unwrap();
    git.branchless("worktree", &["add", "side", "heads(branches())"])?;

    let stdout = git.smartlog()?;
    assert!(stdout.contains("side"), "smartlog output was: {stdout}");
    assert!(stdout.contains(&master_oid.to_string()[..7]));

    Ok(())
}

#[test]
fn test_wt_sw_fails_when_branch_is_not_active() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    let (stdout, stderr) = git.run_with_options(
        &["wt", "sw", "topic"],
        &GitRunOptions {
            expected_exit_code: 1,
            ..Default::default()
        },
    )?;

    assert_eq!(stdout, "");
    assert!(stderr.contains("Branch 'topic' is not active in any linked worktree."));

    Ok(())
}

#[test]
fn test_wt_sw_lists_worktrees_without_target() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    let (_stdout, _stderr) = git.run(&["wt", "add", "topic-wt", "topic"])?;

    let (stdout, stderr) = git.run(&["wt", "sw"])?;

    assert_eq!(stderr, "");
    assert!(stdout.contains("master"), "stdout was: {stdout}");
    assert!(stdout.contains("topic-wt"), "stdout was: {stdout}");
    assert!(stdout.contains("<repo-path>"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_sw_fails_cleanly_for_unresolved_target() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;

    let (stdout, stderr) = git.run_with_options(
        &["wt", "sw", "definitely-missing-target"],
        &GitRunOptions {
            expected_exit_code: 1,
            ..Default::default()
        },
    )?;

    assert_eq!(stdout, "");
    assert!(stderr.contains("Could not resolve switch target 'definitely-missing-target':"));

    Ok(())
}

#[test]
fn test_wt_add_create_sanitizes_worktree_name() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    let (stdout, _stderr) = git.run(&["wt", "add", "feature/topic", "topic"])?;
    assert!(stdout.contains("Created worktree at:"));
    assert!(stdout.contains("feature-topic"));
    assert!(!stdout.contains("\ncd '"), "stdout was: {stdout}");

    let (stdout, _stderr) = git.run(&["worktree", "list", "--porcelain"])?;
    assert!(stdout.contains("branch refs/heads/topic"));
    assert!(stdout.contains("feature-topic"));

    Ok(())
}

#[cfg(unix)]
#[test]
#[ignore = "skim alternate-screen UI is not stable under the PTY test harness"]
fn test_wt_sw_interactive_selects_existing_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    git.run(&["wt", "add", "topic-wt", "topic"])?;

    let exit_status = run_in_pty(
        &git,
        "worktree",
        &["sw", "-i"],
        &[
            PtyAction::Write("topic-wt"),
            PtyAction::Write(CARRIAGE_RETURN),
        ],
    )?;
    assert!(exit_status.success());

    Ok(())
}

#[test]
fn test_wt_sw_outputs_cd_command() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    let (stdout, _stderr) = git.run(&["wt", "add", "topic-wt", "topic"])?;
    assert!(stdout.contains("Created worktree at:"));

    let (stdout, _stderr) = git.run(&["wt", "sw", "topic"])?;
    assert!(stdout.starts_with("cd '"), "stdout was: {stdout}");
    assert!(stdout.contains("topic-wt"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_sw_resolves_worktree_name() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.detach_head()?;
    let test1_oid = git.commit_file("test1", 1)?;
    git.run(&["checkout", "master"])?;
    git.run(&["wt", "add", "manual-reload", &test1_oid.to_string()])?;

    let (stdout, _stderr) = git.run(&["wt", "sw", "manual-reload"])?;
    assert!(stdout.starts_with("cd '"), "stdout was: {stdout}");
    assert!(stdout.contains("manual-reload"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_finish_resolves_worktree_path() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let worktree_root = set_worktree_root(&git)?;

    git.detach_head()?;
    let test1_oid = git.commit_file("test1", 1)?;
    git.run(&["checkout", "master"])?;
    git.run(&["wt", "add", "manual-reload", &test1_oid.to_string()])?;

    let repo_name = git
        .repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap();
    let worktree_path = std::path::Path::new(&worktree_root)
        .join(repo_name)
        .join("manual-reload");
    let worktree_path = worktree_path.to_string_lossy().to_string();
    let (stdout, _stderr) = git.run(&["wt", "finish", &worktree_path])?;
    assert!(stdout.contains("Finished worktree"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_finish_outputs_cd_command_when_finishing_current_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;

    let worktree_wrapper = make_git_worktree(&git, "topic-wt")?;
    let worktree = &worktree_wrapper.worktree;
    let expected_main_worktree_path = std::fs::canonicalize(&git.repo_path)?;

    let output = Command::new(&worktree.path_to_git)
        .current_dir(&worktree.repo_path)
        .args(["wt", "finish"])
        .env_clear()
        .envs(worktree.get_base_env(0))
        .output()?;
    assert!(output.status.success(), "output was: {output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let expected_cd_command = format!("cd '{}/'\n", expected_main_worktree_path.to_string_lossy());
    assert!(
        stdout.starts_with(&expected_cd_command),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("Finished worktree"), "stdout was: {stdout}");
    assert!(!worktree.repo_path.exists());

    Ok(())
}
fn test_wt_list_uses_smartlog_for_worktrees() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.detach_head()?;
    let test1_oid = git.commit_file("test1", 1)?;
    git.run(&["checkout", "master"])?;
    git.run(&["wt", "add", "side", &test1_oid.to_string()])?;

    let (stdout, _stderr) = git.run(&["wt", "list"])?;
    assert!(
        stdout.contains(&test1_oid.to_string()[..7]),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("test-worktrees"), "stdout was: {stdout}");
    assert!(stdout.contains("⎇ side"), "stdout was: {stdout}");
    assert!(stdout.contains("create test1.txt"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_add_runs_post_create_hook_from_config() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let worktree_root = set_worktree_root(&git)?;

    git.run(&[
        "config",
        "branchless.worktree.postCreateHook",
        "printf '%s' \"$BRANCHLESS_WORKTREE_NAME\" > .post-create-name && pwd > .post-create-pwd",
    ])?;

    git.run(&["wt", "add", "topic-wt"])?;

    let repo_name = git
        .repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap();
    let worktree_path = std::path::Path::new(&worktree_root)
        .join(repo_name)
        .join("topic-wt");
    let canonical_worktree_path = std::fs::canonicalize(&worktree_path)?;

    let recorded_name = std::fs::read_to_string(worktree_path.join(".post-create-name"))?;
    assert_eq!(recorded_name, "topic-wt");

    let recorded_pwd = std::fs::read_to_string(worktree_path.join(".post-create-pwd"))?;
    assert_eq!(
        recorded_pwd.trim_end(),
        canonical_worktree_path.to_string_lossy()
    );

    Ok(())
}

#[test]
fn test_wt_add_outputs_cd_command_with_flag() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    let (stdout, _stderr) = git.run(&["wt", "add", "--cd", "topic-wt"])?;
    assert!(
        stdout.contains("Created worktree at:"),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("\ncd '"), "stdout was: {stdout}");
    assert!(stdout.contains("topic-wt"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_add_outputs_cd_command_with_config() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;
    git.run(&["config", "branchless.worktree.add.cd", "true"])?;

    let (stdout, _stderr) = git.run(&["wt", "add", "topic-wt"])?;
    assert!(
        stdout.contains("Created worktree at:"),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("\ncd '"), "stdout was: {stdout}");
    assert!(stdout.contains("topic-wt"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_add_no_cd_flag_overrides_config() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;
    git.run(&["config", "branchless.worktree.add.cd", "true"])?;

    let (stdout, _stderr) = git.run(&["wt", "add", "--no-cd", "topic-wt"])?;
    assert!(
        stdout.contains("Created worktree at:"),
        "stdout was: {stdout}"
    );
    assert!(!stdout.contains("\ncd '"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_add_cd_flag_overrides_config() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;
    git.run(&["config", "branchless.worktree.add.cd", "false"])?;

    let (stdout, _stderr) = git.run(&["wt", "add", "--cd", "topic-wt"])?;
    assert!(
        stdout.contains("Created worktree at:"),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("\ncd '"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_smartlog_shows_current_and_home_worktree_annotations() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;

    git.commit_file("test1", 1)?;
    git.run(&["branch", "topic"])?;

    let repo_name = git
        .repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_owned();

    let worktree_wrapper = make_git_worktree(&git, "topic-wt")?;
    let worktree = worktree_wrapper.worktree;
    worktree.run(&["checkout", "topic"])?;

    let stdout = worktree.smartlog()?;
    assert!(
        stdout.contains(&format!("(⎇ {repo_name}, ᐅ topic-wt)"))
            || stdout.contains(&format!("(ᐅ topic-wt, ⎇ {repo_name})")),
        "smartlog should show both current and home worktrees: {stdout}"
    );

    Ok(())
}
