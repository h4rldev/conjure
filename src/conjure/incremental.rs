//! Per-object header dependencies for incremental rebuilds.
//!
//! Each compile writes a transient depfile (GNU `-MMD -MF`, MSVC
//! `/sourceDependencies`); `build.rs` parses it into an object -> files map and
//! deletes it, so no compiler-specific file lingers. The map is persisted as
//! KDL and read on the next build to decide what a changed header invalidates.

/***********************************************************************/

use serde::{Deserialize, Serialize};
use std::{
  collections::{BTreeSet, HashMap},
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

/// Every object's depfile contents, keyed by absolute object path.
#[derive(Serialize, Deserialize, Default)]
pub struct DepGraph {
  pub objects: HashMap<PathBuf, Vec<PathBuf>>,
}

impl DepGraph {
  fn path(dir: &Path, profile: &str) -> PathBuf {
    dir
      .join(".conjure")
      .join("build")
      .join("objdeps")
      .join(format!("{profile}.kdl"))
  }

  pub fn files(&self) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for deps in self.objects.values() {
      set.extend(deps.iter().cloned());
    }

    set.into_iter().collect()
  }

  pub fn load(dir: &Path, profile: &str) -> Self {
    fs::read_to_string(Self::path(dir, profile))
      .ok()
      .and_then(|s| kdl::de::from_str(&s).ok())
      .unwrap_or_default()
  }

  pub fn save(&self, dir: &Path, profile: &str) {
    let path = Self::path(dir, profile);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut doc) = kdl::se::to_document(self) {
      let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
      doc.autoformat_config(&cfg);
      let _ = fs::write(path, super::proj_write::fix_braces(&doc.to_string()));
    }
  }
}
