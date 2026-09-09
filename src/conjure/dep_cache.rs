//! Disposable, per-project artifact cache for built dependencies.
//!
//! The lockfile (`lock.rs`) is the source of truth for *what* a dependency
//! points at; this cache only remembers *which built library* satisfied a
//! (scope, name, key) triple, so a rebuild can be skipped. It is safe to delete
//! at any time - a miss just rebuilds.
//!
//! Entries are keyed by a project **scope** (the project dir relative to the
//! invocation root: `"."` for the top-level project, `"libs/sub1"` for a
//! co-built sibling) so projects sharing the root cache never read each other's
//! artifacts. The per-dependency key folds the source fingerprint / pinned
//! commit together with the linkage tag (`:s` static / `:d` dynamic), so a build
//! that flips static<->dynamic misses the other's cached artifact instead of
//! reusing it.

/***********************************************************************/

use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

const CACHE_FILE: &str = ".conjure/build/deps.kdl";

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CachedDep {
  key: String,
  lib: PathBuf,
}

/// Cached artifact paths, grouped `scope -> dependency -> entry`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DepCache {
  projects: HashMap<String, HashMap<String, CachedDep>>,
}

impl DepCache {
  /// The cached library for `name` in `scope`, if it was built from `key` and
  /// the artifact still exists on disk.
  ///
  /// The existence check is what makes deleting `bin/`/`lib/` (or a dep's build
  /// output) correctly force a rebuild rather than a stale hit.
  pub fn hit(&self, scope: &str, name: &str, key: &str) -> Option<PathBuf> {
    self
      .projects
      .get(scope)?
      .get(name)
      .filter(|c| c.key == key && c.lib.exists())
      .map(|c| c.lib.clone())
  }

  /// Record that `name` in `scope` was built from `key`, producing `lib`.
  pub fn insert(&mut self, scope: &str, name: &str, key: String, lib: PathBuf) {
    self
      .projects
      .entry(scope.to_string())
      .or_default()
      .insert(name.to_string(), CachedDep { key, lib });
  }

  /// Load the cache rooted at `base`, or an empty cache if it is absent or
  /// unreadable. Never fails: a corrupt cache is treated as a cold cache.
  pub fn load(base: &Path) -> Self {
    fs::read_to_string(base.join(CACHE_FILE))
      .ok()
      .and_then(|s| kdl::de::from_str(&s).ok())
      .unwrap_or_default()
  }

  /// Write the cache under `base`. Best-effort: I/O and serialization errors are
  /// ignored, since a lost cache only costs a rebuild.
  pub fn save(&self, base: &Path) {
    let path = base.join(CACHE_FILE);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut doc) = kdl::se::to_document(self) {
      let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
      doc.autoformat_config(&cfg);

      let text = super::proj_write::fix_braces(&doc.to_string());
      let _ = fs::write(path, text);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::DepCache;
  use std::path::PathBuf;

  #[test]
  fn hit_keyed_by_scope_name_and_key() {
    let existing = std::env::current_exe().unwrap();
    let mut cache = DepCache::default();
    cache.insert(".", "dep", "k1".into(), existing.clone());

    assert_eq!(cache.hit(".", "dep", "k1"), Some(existing.clone()));
    assert_eq!(cache.hit(".", "dep", "k2"), None); // key mismatch
    assert_eq!(cache.hit("sub", "dep", "k1"), None); // scope mismatch
    assert_eq!(cache.hit(".", "other", "k1"), None); // name mismatch
  }

  #[test]
  fn hit_misses_when_lib_missing() {
    let mut cache = DepCache::default();
    cache.insert(
      ".",
      "dep",
      "k".into(),
      PathBuf::from("/nonexistent/libdep.a"),
    );

    assert_eq!(cache.hit(".", "dep", "k"), None);
  }
}
