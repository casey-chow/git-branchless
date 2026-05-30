//! A [`NodeDescriptor`] that annotates each commit with the status of its
//! associated GitHub pull request, as reported by the `gh` command-line tool.

use std::collections::HashMap;
use std::fmt::Write;

use cursive_core::theme::BaseColor;
use cursive_core::utils::markup::StyledString;
use git_branchless_submit::github::query_pull_request_infos;
use git_branchless_submit::{PullRequestInfo, StatusCheck};
use lib::core::config::get_commit_descriptors_github_status;
use lib::core::effects::Effects;
use lib::core::formatting::{Glyphs, StyledStringBuilder};
use lib::core::node_descriptors::{NodeDescriptor, NodeObject};
use lib::git::{GitRunInfo, NonZeroOid, Repo, SerializedNonZeroOid};
use tracing::instrument;

/// Display the GitHub pull-request status for each commit in the supersmartlog.
#[derive(Debug)]
pub struct GithubStatusDescriptor {
    is_enabled: bool,
    pull_requests_by_oid: HashMap<NonZeroOid, PullRequestInfo>,
}

impl GithubStatusDescriptor {
    /// Constructor. Queries GitHub (via `gh`) for the pull requests in the
    /// current repository up front, since [`NodeDescriptor::describe_node`]
    /// does not have access to the effects/network.
    ///
    /// If the query fails (e.g. `gh` is not installed, the user is not
    /// authenticated, or this is not a GitHub repository), a warning is printed
    /// and the descriptor renders nothing, rather than aborting the command.
    #[instrument(skip(effects, git_run_info, repo))]
    pub fn new(
        effects: &Effects,
        git_run_info: &GitRunInfo,
        repo: &Repo,
    ) -> eyre::Result<Self> {
        if !get_commit_descriptors_github_status(repo)? {
            return Ok(GithubStatusDescriptor {
                is_enabled: false,
                pull_requests_by_oid: Default::default(),
            });
        }

        let pull_request_infos = match query_pull_request_infos(effects, git_run_info) {
            Ok(Ok(pull_request_infos)) => pull_request_infos,
            Ok(Err(_)) | Err(_) => {
                writeln!(
                    effects.get_error_stream(),
                    "warning: could not query GitHub pull request status; \
                     rendering smartlog without it.\n\
                     hint: ensure the `gh` command-line tool is installed and \
                     authenticated, and that this is a GitHub repository."
                )?;
                return Ok(GithubStatusDescriptor {
                    is_enabled: false,
                    pull_requests_by_oid: Default::default(),
                });
            }
        };

        let pull_requests_by_oid = pull_request_infos
            .into_values()
            .map(|pull_request_info| {
                let SerializedNonZeroOid(oid) = pull_request_info.head_ref_oid;
                (oid, pull_request_info)
            })
            .collect();
        Ok(GithubStatusDescriptor {
            is_enabled: true,
            pull_requests_by_oid,
        })
    }
}

impl NodeDescriptor for GithubStatusDescriptor {
    #[instrument]
    fn describe_node(
        &mut self,
        _glyphs: &Glyphs,
        object: &NodeObject,
    ) -> eyre::Result<Option<StyledString>> {
        if !self.is_enabled {
            return Ok(None);
        }
        let commit = match object {
            NodeObject::Commit { commit } => commit,
            NodeObject::GarbageCollected { oid: _ } => return Ok(None),
        };
        let pull_request_info = match self.pull_requests_by_oid.get(&commit.get_oid()) {
            Some(pull_request_info) => pull_request_info,
            None => return Ok(None),
        };
        Ok(Some(render_pull_request_status(pull_request_info)))
    }
}

/// The rolled-up status of a pull request's CI checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckRollup {
    /// At least one check failed.
    Failing,
    /// No checks failed, but at least one is still running or queued.
    Pending,
    /// All checks succeeded (and there was at least one).
    Passing,
    /// There were no checks.
    None,
}

/// Roll up the individual CI checks into a single status. GitHub returns either
/// legacy commit statuses (which populate `state`) or check runs (which
/// populate `status` and `conclusion`), so we handle both.
fn roll_up_checks(checks: &[StatusCheck]) -> CheckRollup {
    let mut any = false;
    let mut failing = false;
    let mut pending = false;
    for check in checks {
        any = true;

        // A check run that has not yet completed is still pending.
        if matches!(check.status.as_deref(), Some(status) if !status.eq_ignore_ascii_case("COMPLETED"))
        {
            pending = true;
            continue;
        }

        let state = check.conclusion.as_deref().or(check.state.as_deref());
        match state {
            Some(state)
                if state.eq_ignore_ascii_case("SUCCESS")
                    || state.eq_ignore_ascii_case("NEUTRAL")
                    || state.eq_ignore_ascii_case("SKIPPED") => {}
            Some(state)
                if state.eq_ignore_ascii_case("PENDING")
                    || state.eq_ignore_ascii_case("EXPECTED")
                    || state.eq_ignore_ascii_case("QUEUED")
                    || state.eq_ignore_ascii_case("IN_PROGRESS") =>
            {
                pending = true;
            }
            // FAILURE, ERROR, TIMED_OUT, CANCELLED, ACTION_REQUIRED, etc.
            Some(_) => failing = true,
            None => {}
        }
    }

    if !any {
        CheckRollup::None
    } else if failing {
        CheckRollup::Failing
    } else if pending {
        CheckRollup::Pending
    } else {
        CheckRollup::Passing
    }
}

/// Render a single pull request's status as it appears inline in the
/// supersmartlog, e.g. `#178 Approved Checks passing`.
fn render_pull_request_status(pull_request_info: &PullRequestInfo) -> StyledString {
    let PullRequestInfo {
        number,
        closed,
        is_draft,
        state,
        review_decision,
        status_check_rollup,
        ..
    } = pull_request_info;

    let mut builder = StyledStringBuilder::new();
    builder = builder.append_plain(format!("#{number}"));

    let state = state.to_ascii_uppercase();
    let is_open = if state.is_empty() {
        !closed
    } else {
        state == "OPEN"
    };

    if state == "MERGED" {
        builder = builder
            .append_plain(" ")
            .append_styled("Merged", BaseColor::Magenta.light());
    } else if state == "CLOSED" || (state.is_empty() && *closed) {
        builder = builder
            .append_plain(" ")
            .append_styled("Closed", BaseColor::Red.light());
    } else if *is_draft {
        builder = builder
            .append_plain(" ")
            .append_styled("Draft", BaseColor::White.dark());
    }

    // Review decision and CI status are only meaningful for open pull requests.
    if is_open {
        match review_decision.to_ascii_uppercase().as_str() {
            "APPROVED" => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Approved", BaseColor::Green.light());
            }
            "CHANGES_REQUESTED" => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Changes requested", BaseColor::Red.light());
            }
            "REVIEW_REQUIRED" => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Review required", BaseColor::Yellow.light());
            }
            _ => {}
        }

        match roll_up_checks(status_check_rollup) {
            CheckRollup::Passing => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Checks passing", BaseColor::Green.light());
            }
            CheckRollup::Failing => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Checks failing", BaseColor::Red.light());
            }
            CheckRollup::Pending => {
                builder = builder
                    .append_plain(" ")
                    .append_styled("Checks pending", BaseColor::Yellow.light());
            }
            CheckRollup::None => {}
        }
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(state: Option<&str>, status: Option<&str>, conclusion: Option<&str>) -> StatusCheck {
        StatusCheck {
            state: state.map(|s| s.to_owned()),
            status: status.map(|s| s.to_owned()),
            conclusion: conclusion.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn test_roll_up_checks_empty() {
        assert_eq!(roll_up_checks(&[]), CheckRollup::None);
    }

    #[test]
    fn test_roll_up_checks_commit_statuses() {
        assert_eq!(
            roll_up_checks(&[check(Some("SUCCESS"), None, None)]),
            CheckRollup::Passing
        );
        assert_eq!(
            roll_up_checks(&[
                check(Some("SUCCESS"), None, None),
                check(Some("FAILURE"), None, None),
            ]),
            CheckRollup::Failing
        );
        assert_eq!(
            roll_up_checks(&[
                check(Some("SUCCESS"), None, None),
                check(Some("PENDING"), None, None),
            ]),
            CheckRollup::Pending
        );
    }

    #[test]
    fn test_roll_up_checks_check_runs() {
        assert_eq!(
            roll_up_checks(&[check(None, Some("COMPLETED"), Some("SUCCESS"))]),
            CheckRollup::Passing
        );
        assert_eq!(
            roll_up_checks(&[check(None, Some("IN_PROGRESS"), None)]),
            CheckRollup::Pending
        );
        assert_eq!(
            roll_up_checks(&[check(None, Some("COMPLETED"), Some("FAILURE"))]),
            CheckRollup::Failing
        );
        // A failure dominates a pending check.
        assert_eq!(
            roll_up_checks(&[
                check(None, Some("IN_PROGRESS"), None),
                check(None, Some("COMPLETED"), Some("FAILURE")),
            ]),
            CheckRollup::Failing
        );
    }
}
