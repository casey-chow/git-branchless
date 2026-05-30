//! Display a smartlog annotated with each commit's GitHub pull-request status.
//!
//! This is like the `smartlog` command, but additionally queries GitHub (via
//! the `gh` command-line tool) for the status of the pull requests associated
//! with the displayed commits. Because that requires a network request, it is a
//! separate command from `smartlog`, which is meant to be fast.

#![warn(missing_docs)]
#![warn(
    clippy::all,
    clippy::as_conversions,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro
)]
#![allow(clippy::too_many_arguments, clippy::blocks_in_conditions)]

mod github_status;

use git_branchless_invoke::CommandContext;
use git_branchless_opts::{SmartlogArgs, SupersmartlogArgs};
use git_branchless_smartlog::{SmartlogOptions, smartlog};
use lib::core::node_descriptors::NodeDescriptor;
use lib::git::Repo;
use lib::util::EyreExitOr;
use tracing::instrument;

use github_status::GithubStatusDescriptor;

/// `supersmartlog` command.
#[instrument]
pub fn command_main(ctx: CommandContext, args: SupersmartlogArgs) -> EyreExitOr<()> {
    let CommandContext {
        effects,
        git_run_info,
    } = ctx;
    let SupersmartlogArgs { smartlog_args } = args;
    let SmartlogArgs {
        event_id,
        revset,
        resolve_revset_options,
        reverse,
        exact,
    } = smartlog_args;

    let repo = Repo::from_dir(&git_run_info.working_directory)?;
    let mut github_status_descriptor =
        GithubStatusDescriptor::new(&effects, &git_run_info, &repo)?;
    let mut extra_descriptors: [&mut dyn NodeDescriptor; 1] = [&mut github_status_descriptor];

    smartlog(
        &effects,
        &git_run_info,
        SmartlogOptions {
            event_id,
            revset,
            resolve_revset_options,
            reverse,
            exact,
        },
        &mut extra_descriptors,
    )
}
