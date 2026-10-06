//! taid — a local-first terminal UI for managing which commits the main
//! repository records for its submodules.
//!
//! Library layout:
//! - [`gitx`]: the read-only git model — repository discovery, submodule
//!   snapshot, commit history
//! - [`gitops`]: the mutating operations — safe checkout, submodule pointer
//!   staging, branch switching
//! - [`config`]: nested YAML config discovery and typed parameter forms
//! - [`network`]: background fetches and fast-forward pulls using system Git
//! - [`app`]: terminal-free TUI state machine (modes, focus, popups, input)
//! - [`ui`]: renders `app` state and registers clickable regions
//! - [`event`]: the input loop that ties them together
//! - [`term`]: terminal lifecycle (alternate screen, raw mode, panic hook)

pub mod app;
pub mod config;
pub mod event;
pub mod gitops;
pub mod gitx;
pub mod network;
pub mod term;

mod ui;

/// Test fixtures: real repositories built with the git2 API. Exposed for
/// the integration tests in `tests/`; not part of the public interface.
#[doc(hidden)]
pub mod fixture;
