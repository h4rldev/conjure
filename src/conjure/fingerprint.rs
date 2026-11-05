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
  compile::{
    C_SRCS, CPP_SRCS, collect_sources, find_sources, test_source_roots,
  },
  deps::local_dep_fingerprint,
  diag::ReadPath,
  proj_parse::{Dependency, Language, Project},
};
use miette::Result;
use serde::{Deserialize, Serialize};
use std::{
  collections::{BTreeMap, HashMap, HashSet},
  fs,
  hash::{DefaultHasher, Hash, Hasher},
  io::Read,
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
fn hash_file(path: &Path, size: u64) -> Result<u64> {
  let mut file = fs::File::open(path).map_err(|source| ReadPath {
    path: path.to_path_buf(),
    source,
  })?;

  let mut h = DefaultHasher::new();
  (size as usize).hash(&mut h);
  let mut buf = [0u8; 64 * 1024];
  loop {
    let n = file.read(&mut buf).map_err(|source| ReadPath {
      path: path.to_path_buf(),
      source,
    })?;
    if n == 0 {
      break;
    }
    h.write(&buf[..n]);
  }
  Ok(h.finish())
}

fn file_fingerprint_inner(
  cache: &mut FileCache,
  paths: &[PathBuf],
  skip_missing: bool,
) -> Result<Vec<String>> {
  let mut out = Vec::with_capacity(paths.len());
  for path in paths {
    let md = if skip_missing {
      match fs::metadata(path) {
        Ok(md) => md,
        Err(_) => continue,
      }
    } else {
      fs::metadata(path).map_err(|source| ReadPath {
        path: path.to_path_buf(),
        source,
      })?
    };

    let size = md.len();
    let mtime = mtime_nanos(&md);
    let hash = match cache.files.get(path) {
      Some(entry) if entry.size == size && entry.mtime == mtime => entry.hash,
      _ => {
        let hash = hash_file(path, size)?;
        cache
          .files
          .insert(path.clone(), FileEntry { size, mtime, hash });
        hash
      }
    };

    out.push(format!("file:{}:{hash:016x}", path.display()));
  }
  out.sort();
  Ok(out)
}

/// Like [`file_fingerprint`], but a path that no longer exists is skipped
/// instead of erroring: a depfile can name a since-deleted header.
fn file_fingerprint_opt(
  cache: &mut FileCache,
  paths: &[PathBuf],
) -> Result<Vec<String>> {
  file_fingerprint_inner(cache, paths, true)
}

/// Stable `path:hash` lines for `paths`, sorted so the result does not depend on
/// directory-walk order. `cache` supplies hashes for files whose size and mtime
/// are unchanged, so only newly-touched files are read.
fn file_fingerprint(
  cache: &mut FileCache,
  paths: &[PathBuf],
) -> Result<Vec<String>> {
  file_fingerprint_inner(cache, paths, false)
}

/// Content fingerprint of a whole directory tree, for a dep whose external
/// build system consumes the tree rather than a declared `src` set. It includes
/// generated artifacts, so it is a coarse key and can churn once after the first
/// build; a conjure dep uses its `src` set instead.
pub fn dir_fingerprint(dir: &Path, cache: &mut FileCache) -> Result<String> {
  let files = collect_sources(dir, &[".".to_string()], &[])?;
  Ok(file_fingerprint(cache, &files)?.join(","))
}

/// Split a fingerprint into its non-file items and its `file:path:hash`
/// entries, so a build can tell which sources changed instead of rebuilding
/// everything. The returned base is the non-file items rejoined, so two
/// fingerprints have equal bases if their non-file inputs match.
pub fn split(fp: &str) -> (String, BTreeMap<PathBuf, u64>) {
  let mut base: Vec<&str> = vec![];
  let mut files = BTreeMap::new();
  for item in fp.split('|') {
    let parsed = item.strip_prefix("file:").and_then(|rest| {
      let (path, hash) = rest.rsplit_once(':')?;
      Some((PathBuf::from(path), u64::from_str_radix(hash, 16).ok()?))
    });
    match parsed {
      Some((path, hash)) => {
        files.insert(path, hash);
      }
      None => base.push(item),
    }
  }
  (base.join("|"), files)
}

/// The fingerprint of `project` built in `dir` under `profile_name`, with the
/// given pinned dependency commits. `cache` carries the previous run's file
/// hashes so unchanged files are not re-read.
pub fn fingerprint(
  project: &Project,
  dir: &Path,
  profile_name: &str,
  dep_commits: &[&str],
  pkg_cflags: &[String],
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

  if compile.export_all.unwrap_or(false) {
    items.push("export-all".into());
  }

  if let Some(inc) = &compile.include {
    items.extend(inc.list());
  }

  if let Some(f) = &compile.c_flags {
    items.push(format!("{:?}", f));
  }

  items.extend(pkg_cflags.iter().filter(|s| !s.is_empty()).cloned());

  if let Some(f) = &compile.ld_flags {
    items.push(format!("{:?}", f));
  }

  items.extend(dep_commits.iter().map(|s| s.to_string()));

  let roots = compile.src_roots();
  let mut sources = collect_sources(dir, &roots, exts)?;

  let excludes = test_source_roots(project, dir);
  if !excludes.is_empty() {
    sources.retain(|src| !excludes.iter().any(|e| src.starts_with(e)));
  }
  items.extend(file_fingerprint(cache, &sources)?);

  let dep_graph = super::incremental::DepGraph::load(dir, profile_name);
  let dep_files = dep_graph.files();

  // Depfiles name the headers actually included. Walk the include dirs only
  // before the first depfile record exists; after that the per-object graph
  // already covers header changes.
  if dep_graph.objects.is_empty()
    && let Some(include) = &compile.include
  {
    for inc in include.list() {
      let mut files = vec![];
      find_sources(&dir.join(inc), &[], &mut files)?;
      items.extend(file_fingerprint(cache, &files)?);
    }
  }

  items.extend(file_fingerprint_opt(cache, &dep_files)?);
  if let Some(deps) = &project.dependencies {
    let mut entries: Vec<(&String, &Dependency)> = deps.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dep) in entries {
      items.push(format!(
        "{name}:cfg:{:?}:{:?}:{:?}:{:?}",
        dep.build, dep.include, dep.pkg_config, dep.src
      ));
      if dep.local.is_some() {
        items.push(format!(
          "{name}:{}",
          local_dep_fingerprint(project, dir, profile_name, name, dep, cache)?
        ));
      }
    }
  }

  Ok(items.join("|"))
}

/// The path in a `file:path:hash` fingerprint item.
fn file_entry_path(item: &str) -> Option<&Path> {
  let rest = item.strip_prefix("file:")?;
  let (path, _) = rest.rsplit_once(':')?;
  Some(Path::new(path))
}

/// Swap the depfile block in `fingerprint` for `new_dep_files`.
///
/// The source and dependency portions are unchanged, so this avoids walking
/// the source tree and re-fingerprinting local dependencies after a compile.
/// `None` means the old depfile entries were not one contiguous block (for
/// example a first build, or a file that is both a source and a depfile);
/// the caller then re-fingerprints from scratch.
pub fn replace_depfiles(
  fingerprint: &str,
  old_dep_files: &[PathBuf],
  new_dep_files: &[PathBuf],
  cache: &mut FileCache,
) -> Result<Option<String>> {
  if old_dep_files.is_empty() {
    return Ok(None);
  }

  let old: HashSet<&Path> =
    old_dep_files.iter().map(PathBuf::as_path).collect();
  let items: Vec<&str> = fingerprint.split('|').collect();
  let mut first = None;
  let mut last = 0;
  let mut count = 0;
  for (i, item) in items.iter().enumerate() {
    if file_entry_path(item).is_some_and(|path| old.contains(path)) {
      first.get_or_insert(i);
      last = i;
      count += 1;
    }
  }

  let Some(first) = first else {
    return Ok(None);
  };
  if last + 1 - first != count {
    return Ok(None);
  }

  let new_entries = file_fingerprint_opt(cache, new_dep_files)?;
  let mut out = Vec::with_capacity(items.len() + new_entries.len());
  out.extend_from_slice(&items[..first]);
  out.extend(new_entries.iter().map(String::as_str));
  out.extend_from_slice(&items[last + 1..]);
  Ok(Some(out.join("|")))
}

#[cfg(test)]
mod tests {
  use super::{FileCache, fingerprint, hash_file, replace_depfiles, split};
  use crate::conjure::proj_parse::{
    Arch, Compile, Dependency, Flags, Language, Project,
  };
  use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
  };

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
    std::fs::create_dir_all(base.join("deps/a/src")).unwrap();
    std::fs::create_dir_all(base.join("deps/b/src")).unwrap();
    std::fs::write(base.join("deps/a/src/a.c"), "").unwrap();
    std::fs::write(base.join("deps/b/src/b.c"), "").unwrap();

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
      fingerprint(&a, &base, "default", &[], &[], &mut FileCache::default())
        .unwrap(),
      fingerprint(&b, &base, "default", &[], &[], &mut FileCache::default())
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
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();
    assert_eq!(cache.files.len(), 1, "one source cached");

    // A second run reuses the cached hash and yields the same fingerprint.
    let second =
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();
    assert_eq!(first, second);

    // Editing the content changes the fingerprint. Use a different length so
    // this does not depend on mtime granularity (coarse on some filesystems).
    std::fs::write(src.join("main.c"), "int main(void){return 42;}").unwrap();
    let third =
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();
    assert_ne!(first, third);

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn fingerprint_includes_single_file_sources() {
    let base =
      std::env::temp_dir().join(format!("conjure_fs1_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("a.c"), "int a;").unwrap();
    std::fs::write(base.join("b.c"), "int b;").unwrap();

    let mut project = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(Default::default()),
      ..Default::default()
    };
    project.compile.as_mut().unwrap().src =
      Some(Flags::Append(vec!["a.c".into()]));

    let mut cache = FileCache::default();
    let first =
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();

    // A file the project doesn't list changes nothing.
    std::fs::write(base.join("b.c"), "int b = 1;").unwrap();
    let second =
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();
    assert_eq!(first, second);

    // Editing the listed file invalidates.
    std::fs::write(base.join("a.c"), "int a = 1;").unwrap();
    let third =
      fingerprint(&project, &base, "default", &[], &[], &mut cache).unwrap();
    assert_ne!(first, third);

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn fingerprint_covers_optional_fields() {
    let base =
      std::env::temp_dir().join(format!("conjure_fpo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("src")).unwrap();
    std::fs::create_dir_all(base.join("include")).unwrap();
    std::fs::write(base.join("src/main.c"), "int main(void){return 0;}")
      .unwrap();
    std::fs::write(base.join("include/x.h"), "#pragma once").unwrap();

    let compile = Compile {
      cc: Some("cc".into()),
      linker: Some("gcc".into()),
      standard: Some("c11".into()),
      arch: Some(Arch::X86_64),
      include: Some(Flags::Append(vec!["include".into()])),
      c_flags: Some(Flags::Append(vec!["-Wall".into()])),
      ld_flags: Some(Flags::Append(vec!["-flto".into()])),
      ..Default::default()
    };
    let full = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(compile),
      ..Default::default()
    };
    let fp =
      fingerprint(&full, &base, "default", &[], &[], &mut FileCache::default())
        .unwrap();
    assert!(!fp.is_empty());

    // The all-None shape takes the other side of every `if let Some`.
    let bare = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(Compile::default()),
      ..Default::default()
    };
    fingerprint(&bare, &base, "default", &[], &[], &mut FileCache::default())
      .unwrap();

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn streamed_hash_matches_in_memory_hash() {
    let path =
      std::env::temp_dir().join(format!("conjure_hash_{}", std::process::id()));
    let bytes = b"hello world";
    std::fs::write(&path, bytes).unwrap();

    let mut expected = DefaultHasher::new();
    bytes.to_vec().hash(&mut expected);

    assert_eq!(
      hash_file(&path, bytes.len() as u64).unwrap(),
      expected.finish()
    );
    let _ = std::fs::remove_file(&path);
  }

  #[test]
  fn split_separates_file_lines() {
    let fp = "default|cc|file:/a/x.c:00000000000000ff|file:/a/y.c:00000000000000aa|Append([\"-O2\"])";
    let (base, files) = split(fp);
    assert_eq!(base, "default|cc|Append([\"-O2\"])");
    assert_eq!(files.get(Path::new("/a/x.c")), Some(&0xff));
    assert_eq!(files.get(Path::new("/a/y.c")), Some(&0xaa));
  }

  #[test]
  fn replace_depfiles_swaps_only_the_depfile_block() {
    let dir = std::env::temp_dir()
      .join(format!("conjure_replace_depfiles_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.h");
    let keep = dir.join("keep.h");
    let new = dir.join("new.h");
    std::fs::write(&old, "old").unwrap();
    std::fs::write(&keep, "keep").unwrap();
    std::fs::write(&new, "new").unwrap();

    let source = dir.join("source.c");
    let fp = format!(
      "profile|file:{}:{:016x}|file:{}:{:016x}|file:{}:{:016x}|deps",
      source.display(),
      1u64,
      old.display(),
      2u64,
      keep.display(),
      3u64,
    );
    let mut cache = FileCache::default();
    let replaced = replace_depfiles(
      &fp,
      &[old.clone(), keep.clone()],
      &[keep.clone(), new.clone()],
      &mut cache,
    )
    .unwrap()
    .unwrap();

    assert_eq!(split(&fp).0, split(&replaced).0);
    let new_text = new.display().to_string();
    let old_text = old.display().to_string();
    let source_text = source.display().to_string();
    assert!(replaced.contains(new_text.as_str()));
    assert!(!replaced.contains(old_text.as_str()));
    assert!(replaced.contains(source_text.as_str()));

    let files = split(&replaced).1;
    assert!(files.contains_key(&keep));
    assert!(files.contains_key(&new));
    assert!(!files.contains_key(&old));
  }

  #[test]
  fn pkg_cflags_change_the_fingerprint() {
    let base = std::env::temp_dir()
      .join(format!("conjure_fp_pkg_{}", std::process::id()));
    std::fs::create_dir_all(base.join("src")).unwrap();
    std::fs::write(base.join("src/main.c"), "").unwrap();

    let project = Project {
      name: "x".into(),
      language: Language::C,
      compile: Some(Default::default()),
      ..Default::default()
    };

    let bare = fingerprint(
      &project,
      &base,
      "default",
      &[],
      &[],
      &mut FileCache::default(),
    )
    .unwrap();
    let with_pkg = fingerprint(
      &project,
      &base,
      "default",
      &[],
      &["-DX".to_string()],
      &mut FileCache::default(),
    )
    .unwrap();

    assert_ne!(bare, with_pkg);

    let _ = std::fs::remove_dir_all(&base);
  }
}
