//! Pinned dependency revisions (`conjure.lock`).
//!
//! The lockfile records, per remote dependency, the exact commit it resolved to
//! (plus the ref that led there). It is the standing point a build revisits:
//! [`LockFile::load`] reads it, and `deps.rs` checks out the recorded commit so
//! builds are reproducible without network access beyond the first clone.
//! `conjure lock` (offline) and `conjure update` (always fetches) both rewrite
//! it through [`LockFile::lock`]/[`LockFile::save`]; an unlocked dependency is
//! pinned automatically during a build.
//!
//! Local dependencies are intentionally absent: they have no commit to pin, so
//! they are keyed by a source fingerprint in the dep cache instead.

/***********************************************************************/

use super::{
  proj_parse::{Remote, Transport},
  proj_write::fix_braces,
};
use kdl::{FormatConfigBuilder, de as kdl_deserial, se as kdl_serial};
use miette::{IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::Path};

/***********************************************************************/

/// A single pinned dependency: where it came from and the commit to check out.
#[derive(Serialize, Deserialize, Debug)]
pub struct LockedDep {
  pub remote: Option<Remote>,
  pub transport: Option<Transport>,
  #[serde(rename = "ref")]
  pub r#ref: Option<String>,
  pub commit: String,
}

/// The `conjure.lock` contents.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct LockFile {
  lock: HashMap<String, LockedDep>,
}

impl LockFile {
  /// Read a lockfile, or an empty one when `path` does not exist. A missing file
  /// is a normal state (nothing pinned yet), not an error.
  pub fn load(path: impl AsRef<Path>) -> Result<Self> {
    match std::fs::read_to_string(path) {
      Ok(text) => Ok(kdl_deserial::from_str(&text).into_diagnostic()?),
      Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
      Err(e) => Err(e).into_diagnostic(),
    }
  }

  /// Write the lockfile. Serialization goes through the shared brace/indent
  /// fixups so the output matches the rest of the generated KDL.
  pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
    let cfg = FormatConfigBuilder::new().indent("  ").build();

    let mut doc = kdl_serial::to_document(self).into_diagnostic()?;
    doc.autoformat_config(&cfg);

    let text = fix_braces(&doc.to_string());
    fs::write(path, text).into_diagnostic()?;
    Ok(())
  }

  /// Pin `name` to `commit`, replacing any previous entry.
  pub fn lock(
    &mut self,
    name: &str,
    remote: Option<Remote>,
    transport: Option<Transport>,
    r#ref: Option<String>,
    commit: String,
  ) {
    self.lock.insert(
      name.to_string(),
      LockedDep {
        remote,
        transport,
        r#ref,
        commit,
      },
    );
  }

  /// Drop the pin for `name`, e.g. when the dependency is removed.
  pub fn unlock(&mut self, name: &str) {
    self.lock.remove(name);
  }

  /// Keep only the entries whose name satisfies `f`, so the lockfile can be
  /// pruned to match the current manifest.
  pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
    self.lock.retain(|k, _| f(k));
  }

  /// All pins, keyed by dependency name.
  pub fn entries(&self) -> &HashMap<String, LockedDep> {
    &self.lock
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lock_retain_and_unlock() {
    let mut lock = LockFile::default();
    lock.lock("a", None, None, None, "c1".into());
    lock.lock("b", None, None, None, "c2".into());
    assert_eq!(lock.entries().len(), 2);

    lock.unlock("a");
    assert!(!lock.entries().contains_key("a"));
    assert!(lock.entries().contains_key("b"));

    lock.retain(|name| name == "b");
    assert_eq!(lock.entries().len(), 1);
  }

  #[test]
  fn load_missing_file_is_empty() {
    let path = std::env::temp_dir()
      .join(format!("conjure_lock_{}.kdl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert!(LockFile::load(&path).unwrap().entries().is_empty());
  }
}
