//! Build state fingerprinting: a string that changes whenever the inputs to a
//! build change, so `build.rs` can skip a project whose outputs are current.
//!
//! The fingerprint folds the active profile, the compile settings, a content
//! hash of every source and include-dir file, dependency commits, and dependency
//! configuration into one string compared byte-for-byte against
//! `.conjure/build/state.kdl`.
//!
//! Content hashing is kept cheap by [`FileCache`]: it remembers each file's
//! size, mtime, and hash, so a file is only re-read when its stat changed. The
//! fast path assumes mtime granularity is finer than the edit rate; a filesystem
//! with coarse timestamps (e.g. FAT) can miss a same-size edit within one tick.
//!
//! Everything derived from a `HashMap` (dependencies, lock entries) is sorted
//! first: map iteration order is randomized per process, so an unsorted walk
//! would produce a different fingerprint on every run and defeat all caching.

/***********************************************************************/

use super::{
  compile::{C_SRCS, CPP_SRCS, find_sources},
  proj_parse::{Dependency, Language, Project},
};
use miette::{IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  fs,
  hash::{DefaultHasher, Hash, Hasher},
  path::{Path, PathBuf},
  time::UNIX_EPOCH,
};

/***********************************************************************/

const CACHE_FILE: &str = ".conjure/build/fingerprint.kdl";

/// One file's last-seen stat and content hash.
#[derive(Serialize, Deserialize)]
struct FileEntry {
  size: u64,
  mtime: i64,
  hash: u64,
}

/// Per-file hash cache persisted between builds, so unchanged files are not
/// re-read. Deletable at any time: a miss just re-hashes.
#[derive(Serialize, Deserialize, Default)]
pub struct FileCache {
  files: HashMap<PathBuf, FileEntry>,
}

impl FileCache {
  /// Load the cache rooted at `base`, or an empty cache if absent/unreadable.
  pub fn load(base: &Path) -> Self {
    fs::read_to_string(base.join(CACHE_FILE))
      .ok()
      .and_then(|s| kdl::de::from_str(&s).ok())
      .unwrap_or_default()
  }

  /// Write the cache under `base`. Best-effort, like the dep cache: a lost
  /// cache only costs a re-read.
  pub fn save(&self, base: &Path) {
    let path = base.join(CACHE_FILE);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut doc) = kdl::se::to_document(self) {
      let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
      doc.autoformat_config(&cfg);
      let _ = fs::write(path, super::proj_write::fix_braces(&doc.to_string()));
    }
  }
}

/// Nanoseconds since the unix epoch, or 0 if unavailable. A change signal, not a
/// timestamp: only equality matters.
fn mtime_nanos(md: &fs::Metadata) -> i64 {
  md.modified()
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map_or(0, |d| d.as_nanos() as i64)
}

/// Content hash of a file. `DefaultHasher` is fixed-key (not randomized), so the
/// hash is stable across runs; it detects change, it is not a security hash.
fn hash_file(path: &Path) -> Result<u64> {
  let bytes = fs::read(path).into_diagnostic()?;
  let mut h = DefaultHasher::new();
  bytes.hash(&mut h);
  Ok(h.finish())
}

/// Stable `path:hash` lines for `paths`, sorted so the result does not depend on
/// directory-walk order. `cache` supplies hashes for files whose size and mtime
/// are unchanged, so only newly-touched files are read.
fn file_fingerprint(
  cache: &mut FileCache,
  paths: &[PathBuf],
) -> Result<Vec<String>> {
  let mut out = Vec::with_capacity(paths.len());
  for path in paths {
    let md = fs::metadata(path).into_diagnostic()?;
    let size = md.len();
    let mtime = mtime_nanos(&md);

    let hash = match cache.files.get(path) {
      Some(entry) if entry.size == size && entry.mtime == mtime => entry.hash,
      _ => {
        let hash = hash_file(path)?;
        cache
          .files
          .insert(path.clone(), FileEntry { size, mtime, hash });
        hash
      }
    };
    out.push(format!("{}:{hash:016x}", path.display()));
  }
  out.sort();
  Ok(out)
}

/// Fingerprint of an entire directory tree, used as a cache key for a local
/// dependency's sources.
pub fn dir_key(dir: &Path) -> Result<String> {
  let mut files = vec![];
  find_sources(dir, &[], &mut files)?;
  let mut cache = FileCache::default();
  Ok(file_fingerprint(&mut cache, &files)?.join(","))
}

/// The fingerprint of `project` built in `dir` under `profile_name`, with the
/// given pinned dependency commits. `cache` carries the previous run's file
/// hashes so unchanged files are not re-read.
pub fn fingerprint(
  project: &Project,
  dir: &Path,
  profile_name: &str,
  dep_commits: &[&str],
  cache: &mut FileCache,
) -> Result<String> {
  let exts = match project.language {
    Language::C => C_SRCS,
    Language::Cpp => CPP_SRCS,
  };

  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("no compile section"))?;

  let mut items = vec![profile_name.to_string()];
  if let Some(cc) = &compile.cc {
    items.push(cc.clone());
  }

  // `ty` decides library vs binary and, with `link`, shared-object PIC; both
  // change the compile/link line, so both must invalidate.
  items.push(format!("ty:{:?}", project.ty));
  items.push(format!("link:{:?}", project.link));
  if let Some(l) = &compile.linker {
    items.push(l.clone());
  }

  if let Some(std) = &compile.standard {
    items.push(std.clone());
  }

  if let Some(arch) = compile.arch {
    items.push(format!("arch:{arch:?}"));
  }

  if let Some(inc) = &compile.include {
    items.extend(inc.list());
  }

  if let Some(f) = &compile.c_flags {
    items.push(format!("{:?}", f));
  }

  if let Some(f) = &compile.ld_flags {
    items.push(format!("{:?}", f));
  }

  items.extend(dep_commits.iter().map(|s| s.to_string()));

  let roots = compile.src_roots();
  let mut sources = vec![];
  for root in &roots {
    find_sources(&dir.join(root), exts, &mut sources)?;
  }
  items.extend(file_fingerprint(cache, &sources)?);

  if let Some(include) = &compile.include {
    for inc in include.list() {
      let mut files = vec![];
      find_sources(&dir.join(inc), &[], &mut files)?;
      items.extend(file_fingerprint(cache, &files)?);
    }
  }

  if let Some(deps) = &project.dependencies {
    let mut entries: Vec<(&String, &Dependency)> = deps.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dep) in entries {
      items.push(format!(
        "{name}:cfg:{:?}:{:?}:{:?}:{:?}",
        dep.build, dep.include, dep.pkg_config, dep.src
      ));
      if let Some(path) = &dep.local {
        let mut files = vec![];
        find_sources(&dir.join(path), &[], &mut files)?;
        items.push(format!(
          "{name}:{}",
          file_fingerprint(cache, &files)?.join(",")
        ));
      }
    }
  }

  Ok(items.join("|"))
}

#[cfg(test)]
mod tests {
  use super::{FileCache, fingerprint};
  use crate::conjure::proj_parse::{Dependency, Language, Project};
  use std::collections::HashMap;

  fn local_dep(path: &str) -> Dependency {
    Dependency {
      remote: None,
      local: Some(path.into()),
      transport: None,
      build: None,
      include: None,
      src: None,
      pkg_config: None,
      r#ref: None,
    }
  }

  #[test]
  fn fingerprint_is_order_independent_for_deps() {
    let base =
      std::env::temp_dir().join(format!("conjure_fp_{}", std::process::id()));
    std::fs::create_dir_all(base.join("src")).unwrap();
    std::fs::write(base.join("src/main.c"), "").unwrap();
    std::fs::create_dir_all(base.join("deps/a")).unwrap();
    std::fs::create_dir_all(base.join("deps/b")).unwrap();
    std::fs::write(base.join("deps/a/a.c"), "").unwrap();
    std::fs::write(base.join("deps/b/b.c"), "").unwrap();

    let mut a = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(Default::default()),
      ..Default::default()
    };
    let mut b = a.clone();

    // Same deps, inserted into the maps in opposite orders.
    let mut m1 = HashMap::new();
    m1.insert("a".to_string(), local_dep("deps/a"));
    m1.insert("b".to_string(), local_dep("deps/b"));
    a.dependencies = Some(m1);

    let mut m2 = HashMap::new();
    m2.insert("b".to_string(), local_dep("deps/b"));
    m2.insert("a".to_string(), local_dep("deps/a"));
    b.dependencies = Some(m2);

    assert_eq!(
      fingerprint(&a, &base, "default", &[], &mut FileCache::default())
        .unwrap(),
      fingerprint(&b, &base, "default", &[], &mut FileCache::default())
        .unwrap()
    );

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn file_cache_avoids_rereading_unchanged_files() {
    let base =
      std::env::temp_dir().join(format!("conjure_fc_{}", std::process::id()));
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.c"), "int main(void){return 0;}").unwrap();

    let mut project = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(Default::default()),
      ..Default::default()
    };
    project.compile.as_mut().unwrap().cc = Some("cc".into()); // any stable setting

    let mut cache = FileCache::default();
    let first =
      fingerprint(&project, &base, "default", &[], &mut cache).unwrap();
    assert_eq!(cache.files.len(), 1, "one source cached");

    // A second run reuses the cached hash and yields the same fingerprint.
    let second =
      fingerprint(&project, &base, "default", &[], &mut cache).unwrap();
    assert_eq!(first, second);

    // Editing the content changes the fingerprint.
    std::fs::write(src.join("main.c"), "int main(void){return 1;}").unwrap();
    let third =
      fingerprint(&project, &base, "default", &[], &mut cache).unwrap();
    assert_ne!(first, third);

    let _ = std::fs::remove_dir_all(&base);
  }
}
