//! Small `Diagnostic` error types for I/O failures. They name the path and carry
//! a hint while keeping the io error as `#[source]`, so miette renders the
//! `╰─▶` cause chain and the `help` line together (an ad-hoc `miette!` can only
//! do one or the other).

use miette::Diagnostic;
use std::path::PathBuf;

/// `read_dir` failed - usually a mistyped `src`/`include` root.
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("Failed to read `{}`", .path.display())]
#[diagnostic(help("Did you mistype the directory?"))]
pub struct ReadDir {
  pub path: PathBuf,
  #[source]
  pub source: std::io::Error,
}

/// `fs::read`/`fs::metadata` failed on a known file.
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("Failed to read `{}`", .path.display())]
#[diagnostic(help("Likely a permissions issue"))]
pub struct ReadPath {
  pub path: PathBuf,
  #[source]
  pub source: std::io::Error,
}
