use lib::testing::{make_git, make_git_worktree};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_smartlog_shows_detached_linked_worktree() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    git.commit_file("test1", 1)?;

    let _worktree = make_git_worktree(&git, "side")?;

    let stdout = git.smartlog()?;
    assert!(stdout.contains("⎇ side"), "stdout was: {stdout}");

    Ok(())
}

#[test]
fn test_smartlog_shows_current_and_home_worktree_annotations() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    git.commit_file("test1", 1)?;

    let repo_name = git
        .repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_owned();

    let worktree_wrapper = make_git_worktree(&git, "topic-wt")?;
    let worktree = worktree_wrapper.worktree;

    let stdout = worktree.smartlog()?;
    assert!(
        stdout.contains(&format!("ᐅ topic-wt")),
        "stdout should show the current worktree annotation: {stdout}"
    );
    assert!(
        stdout.contains(&format!("⎇ {repo_name}")),
        "stdout should show the home worktree annotation: {stdout}"
    );

    Ok(())
}

#[test]
fn test_smartlog_disambiguates_duplicate_worktree_names() -> eyre::Result<()> {
    let git = make_git()?;
    git.init_repo()?;
    git.commit_file("test1", 1)?;

    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let temp_dir =
        std::env::temp_dir().join(format!("git-branchless-smartlog-worktree-{unique_suffix}"));
    let feature_topic = temp_dir.join("feature").join("topic");
    let bugfix_topic = temp_dir.join("bugfix").join("topic");
    std::fs::create_dir_all(feature_topic.parent().unwrap())?;
    std::fs::create_dir_all(bugfix_topic.parent().unwrap())?;

    git.run(&[
        "worktree",
        "add",
        "--detach",
        feature_topic.to_string_lossy().as_ref(),
    ])?;
    git.run(&[
        "worktree",
        "add",
        "--detach",
        bugfix_topic.to_string_lossy().as_ref(),
    ])?;

    let stdout = git.smartlog()?;
    assert!(stdout.contains("feature/topic"), "stdout was: {stdout}");
    assert!(stdout.contains("bugfix/topic"), "stdout was: {stdout}");

    Ok(())
}
