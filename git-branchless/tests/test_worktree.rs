use lib::testing::{GitRunOptions, make_git, make_git_worktree};

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
fn test_wt_add_sanitizes_worktree_name() -> eyre::Result<()> {
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

#[test]
fn test_wt_add_rejects_branch_active_in_another_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    git.run(&["wt", "add", "topic-wt", "topic"])?;
    let (stdout, stderr) = git.run_with_options(
        &["wt", "add", "topic-again", "topic"],
        &GitRunOptions {
            expected_exit_code: 1,
            ..Default::default()
        },
    )?;

    assert_eq!(stdout, "");
    assert!(stderr.contains("Branch 'topic' is already active in another worktree."));
    assert!(stderr.contains("Use `git wt list` to find that worktree."));

    Ok(())
}

#[test]
fn test_wt_list_shows_worktrees() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let _worktree_root = set_worktree_root(&git)?;

    git.detach_head()?;
    let test1_oid = git.commit_file("test1", 1)?;
    git.run(&["checkout", "master"])?;
    git.run(&["wt", "add", "side", &test1_oid.to_string()])?;

    let (stdout, _stderr) = git.run(&["wt", "ls"])?;
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
fn test_wt_rm_resolves_worktree_path() -> eyre::Result<()> {
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
    let (stdout, _stderr) = git.run(&["wt", "rm", &worktree_path])?;
    assert!(stdout.contains("Removed worktree"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_wt_rm_refuses_current_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;

    let worktree_wrapper = make_git_worktree(&git, "topic-wt")?;
    let worktree = &worktree_wrapper.worktree;
    let (stdout, stderr) = worktree.run_with_options(
        &["wt", "rm"],
        &GitRunOptions {
            expected_exit_code: 1,
            ..Default::default()
        },
    )?;

    assert_eq!(stdout, "");
    assert!(stderr.contains("Refusing to remove the current worktree from inside it."));
    assert!(worktree.repo_path.exists());

    Ok(())
}

#[test]
fn test_wt_rm_force_removes_dirty_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    let worktree_root = set_worktree_root(&git)?;

    git.run(&["branch", "topic"])?;
    git.run(&["wt", "add", "topic-wt", "topic"])?;

    let repo_name = git
        .repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap();
    let worktree_path = std::path::Path::new(&worktree_root)
        .join(repo_name)
        .join("topic-wt");
    std::fs::write(worktree_path.join("dirty.txt"), "dirty\n")?;

    let (_stdout, stderr) = git.run_with_options(
        &["wt", "rm", "topic-wt"],
        &GitRunOptions {
            expected_exit_code: 128,
            ..Default::default()
        },
    )?;
    assert!(
        stderr.contains("contains modified or untracked files"),
        "stderr was: {stderr}"
    );
    assert!(worktree_path.exists());

    let (stdout, _stderr) = git.run(&["wt", "rm", "-f", "topic-wt"])?;
    assert!(stdout.contains("Removed worktree"), "stdout was: {stdout}");
    assert!(!worktree_path.exists());

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
