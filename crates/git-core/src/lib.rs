//! Git CLI runner and output parsers. Spawns the installed `git`; never links it
//! (design: "Git backend: CLI first").

pub mod cli;
pub mod detect;
pub mod error;
pub mod ops;
pub mod parse;
pub mod patch;
pub mod progress;
pub mod repo;
pub mod runner;
pub mod setup;
pub mod watch;

/// Read-only gix fast path (design: optional, behind the GitRepo trait). Not implemented yet.
#[cfg(feature = "gix")]
pub mod gix_repo {}

pub use cli::{CliRepo, LogStream};
pub use detect::{GitBinary, GitVersion};
pub use error::{FailureKind, GitError, Result};
pub use ops::{ApplyTarget, CommitRequest, StashAction};
pub use patch::{Direction, Pick};
pub use progress::Progress;
pub use repo::{DiffTarget, GitRepo, LogQuery};
pub use runner::{CommandLog, CommandRecord, GitCommand, GitOutput, GitRunner, GitStream, Mode};
