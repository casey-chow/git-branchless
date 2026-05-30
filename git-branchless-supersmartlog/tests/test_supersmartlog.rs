use std::collections::HashMap;

use git_branchless_submit::StatusCheck;
use git_branchless_submit::github::testing::MockGithubClient;
use lib::git::GitVersion;
use lib::testing::{Git, GitRunOptions, GitWrapperWithRemoteRepo, make_git_with_remote_repo};

/// Minimum version due to changes in the output of `git push`.
const MIN_VERSION: GitVersion = GitVersion(2, 36, 0);

fn mock_env(git: &Git) -> HashMap<String, String> {
    git.get_base_env(0)
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_str().unwrap().to_string(),
                v.to_str().unwrap().to_string(),
            )
        })
        .chain([(
            git_branchless_submit::github::MOCK_REMOTE_REPO_PATH_ENV_KEY.to_string(),
            git.repo_path.clone().to_str().unwrap().to_owned(),
        )])
        .collect()
}

/// `supersmartlog` should annotate each commit that is the head of a pull
/// request with that pull request's number, review decision, and CI status.
#[test]
fn test_supersmartlog_shows_pull_request_status() -> eyre::Result<()> {
    let GitWrapperWithRemoteRepo {
        temp_dir: _temp_dir,
        original_repo: remote_repo,
        cloned_repo: local_repo,
    } = make_git_with_remote_repo()?;
    if remote_repo.get_version()? < MIN_VERSION {
        return Ok(());
    }

    remote_repo.init_repo()?;
    remote_repo.clone_repo_into(&local_repo, &[])?;

    local_repo.detach_head()?;
    local_repo.commit_file("test1", 1)?;
    local_repo.commit_file("test2", 2)?;
    local_repo.branchless_with_options(
        "submit",
        &["--create", "--forge", "github"],
        &GitRunOptions {
            env: mock_env(&remote_repo),
            ..Default::default()
        },
    )?;

    // Simulate review and CI status, which the mock forge does not populate on
    // creation: the first pull request is approved with passing checks, and the
    // second has changes requested with failing checks.
    let client = MockGithubClient {
        remote_repo_path: remote_repo.repo_path.clone(),
    };
    client.with_state_mut(|state| {
        let pull_request = state
            .pull_requests
            .get_mut("mock-github-username/create-test1-txt")
            .unwrap();
        pull_request.review_decision = "APPROVED".to_string();
        pull_request.status_check_rollup = vec![StatusCheck {
            state: Some("SUCCESS".to_string()),
            status: None,
            conclusion: None,
        }];

        let pull_request = state
            .pull_requests
            .get_mut("mock-github-username/create-test2-txt")
            .unwrap();
        pull_request.review_decision = "CHANGES_REQUESTED".to_string();
        pull_request.status_check_rollup = vec![StatusCheck {
            state: None,
            status: Some("COMPLETED".to_string()),
            conclusion: Some("FAILURE".to_string()),
        }];
        Ok(())
    })?;

    let (stdout, _stderr) = local_repo.branchless_with_options(
        "supersmartlog",
        &[],
        &GitRunOptions {
            env: mock_env(&remote_repo),
            ..Default::default()
        },
    )?;
    insta::assert_snapshot!(stdout, @"
    O f777ecc (master) create initial.txt
    |
    o 62fc20d (mock-github-username/create-test1-txt) #1 Approved Checks passing create test1.txt
    |
    @ 96d1c37 (mock-github-username/create-test2-txt) #2 Changes requested Checks failing create test2.txt
    ");

    Ok(())
}

/// When the GitHub status descriptor is disabled via config, `supersmartlog`
/// should render the same output as a plain `smartlog`.
#[test]
fn test_supersmartlog_status_disabled_by_config() -> eyre::Result<()> {
    let GitWrapperWithRemoteRepo {
        temp_dir: _temp_dir,
        original_repo: remote_repo,
        cloned_repo: local_repo,
    } = make_git_with_remote_repo()?;
    if remote_repo.get_version()? < MIN_VERSION {
        return Ok(());
    }

    remote_repo.init_repo()?;
    remote_repo.clone_repo_into(&local_repo, &[])?;

    local_repo.detach_head()?;
    local_repo.commit_file("test1", 1)?;
    local_repo.branchless_with_options(
        "submit",
        &["--create", "--forge", "github"],
        &GitRunOptions {
            env: mock_env(&remote_repo),
            ..Default::default()
        },
    )?;
    local_repo.run(&[
        "config",
        "branchless.commitDescriptors.githubStatus",
        "false",
    ])?;

    let (stdout, _stderr) = local_repo.branchless_with_options(
        "supersmartlog",
        &[],
        &GitRunOptions {
            env: mock_env(&remote_repo),
            ..Default::default()
        },
    )?;
    insta::assert_snapshot!(stdout, @"
    O f777ecc (master) create initial.txt
    |
    @ 62fc20d (mock-github-username/create-test1-txt) create test1.txt
    ");

    Ok(())
}
