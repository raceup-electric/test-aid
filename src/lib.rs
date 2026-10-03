//! taid — a local-first terminal UI for managing which commits the main
//! repository records for its submodules.
//!
//! Library layout:
//! - [`gitx`]: the read-only git model — repository discovery, submodule
//!   snapshot, commit history
//! - [`gitops`]: the mutating operations — safe checkout, submodule pointer
//!   staging, branch switching. All strictly local and non-destructive:
//!   nothing is fetched, pushed, or rewritten.
//!
//! This crate exists as a library so the operations can be exercised by
//! tests (and, soon, the TUI) while `src/main.rs` stays a thin binary.

pub mod gitops;
pub mod gitx;

#[cfg(test)]
mod fixture;

pub use gitops::{BranchEntry, branches, checkout_oid, set_submodule_pointer, switch_branch};
pub use gitx::{
    CommitInfo, CommitLog, HeadInfo, RepoOverview, SubmoduleEntry, discover, head_info, is_dirty,
    overview,
};
