//! Source discovery and compilation.
//!
//! Turns a project's source roots into a set of compile-command entries (one per
//! file), then runs them in parallel through the resolved [`Toolchain`]. The
//! same [`compile_entries`] output feeds `compile_commands.json`, so the clangd
//! database and the real build cannot drift.
//!
//! Object files are namespaced by profile under `.conjure/build/obj/<profile>/`
//! and named by the source's path relative to the project, so multiple profiles
//! coexist and a file's object is stable across runs.

/***********************************************************************/

use super::{
  build,
  deps::dep_include_dirs,
  diag::{ReadDir, WritePath},
  proj_parse::{Flags, Language, Project},
  toolchain,
  ui::{StepStatus, Ui, mark},
};
use indicatif::ParallelProgressIterator;
use miette::{IntoDiagnostic, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::{
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

pub const C_SRCS: &[&str] = &["c", "C"];
pub const CPP_SRCS: &[&str] = &["cpp", "cc", "cxx", "c++"];

/// A single `clangd`/build entry: the compile invocation for one source.
#[derive(Debug, Serialize)]
pub struct CompileEntry {
  directory: String,
  arguments: Vec<String>,
  file: String,
}

impl CompileEntry {
  pub fn source(&self) -> &str {
    &self.file
  }
}

/// Recursively collect files under `dir` whose extension is in `exts`. An empty
/// `exts` collects every file (used to fingerprint include trees).
pub fn find_sources(
  dir: &Path,
  exts: &[&str],
  out: &mut Vec<PathBuf>,
) -> Result<()> {
  for entry in fs::read_dir(dir).map_err(|source| ReadDir {
    path: dir.to_path_buf(),
    source,
  })? {
    let p = entry.into_diagnostic()?.path();
    if p.is_dir() {
      find_sources(&p, exts, out)?;
    } else if exts.is_empty()
      || p
        .extension()
        .is_some_and(|e| exts.contains(&e.to_str().unwrap_or_default()))
    {
      out.push(p);
    }
  }
  Ok(())
}

/// Expand a project's configured source roots into files: recurse directories,
/// and take an exact-file root as named (no extension filter).
pub fn collect_sources(
  root: &Path,
  roots: &[String],
  exts: &[&str],
) -> Result<Vec<PathBuf>> {
  let mut sources = vec![];
  for root_dir in roots {
    let path = root.join(root_dir);
    if path.is_file() {
      sources.push(path);
    } else {
      find_sources(&path, exts, &mut sources)?;
    }
  }
  Ok(sources)
}

/// Absolute roots of the project's declared test sources. A project's own build
/// skips these so its tests are not compiled into the artifact it produces.
pub fn test_source_roots(project: &Project, root: &Path) -> Vec<PathBuf> {
  project
    .tests
    .as_ref()
    .into_iter()
    .flatten()
    .filter_map(|(_, test)| test.src.as_ref())
    .flat_map(Flags::list)
    .map(|src| root.join(src))
    .collect()
}

/// Build the compile invocation for every source in `project`.
///
/// `root` is the project dir (sources, object paths, and `compile_commands`
/// `directory` all resolve against it); `cache_root` is the invocation root,
/// used only to find dependency include dirs. Returns each object path paired
/// with its entry so the caller can pass the objects straight to the linker.
pub fn compile_entries(
  root: &Path,
  cache_root: &Path,
  project: &Project,
  profile_name: &str,
  pkg_cflags: Vec<String>,
  depfiles: bool,
) -> Result<Vec<(PathBuf, CompileEntry)>> {
  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("No compile section in conjure.kdl"))?;

  let exts = match project.language {
    Language::C => C_SRCS,
    Language::Cpp => CPP_SRCS,
  };

  let roots = compile.src_roots();
  let mut sources = collect_sources(root, &roots, exts)?;

  let excludes = test_source_roots(project, root);
  if !excludes.is_empty() {
    sources.retain(|src| !excludes.iter().any(|e| src.starts_with(e)));
  }

  miette::ensure!(
    !sources.is_empty(),
    miette::miette!(
      help = "Make a main.c or main.cpp file in src/",
      "No source files found in src/"
    )
  );

  let c_flags = compile
    .c_flags
    .as_ref()
    .map(Flags::list)
    .unwrap_or_default();

  let obj_dir = root
    .join(".conjure")
    .join("build")
    .join("obj")
    .join(profile_name);

  fs::create_dir_all(&obj_dir).into_diagnostic()?;

  let tc = toolchain::resolve(compile, &project.language);
  // Objects feeding a shared library must be position-independent.
  let shared_objs = project.is_library() && !project.want_static();

  sources
    .into_iter()
    .map(|src| {
      let rel = src
        .strip_prefix(root)
        .unwrap_or(&src)
        .to_string_lossy()
        .replace(['/', '\\'], "_");

      let obj = obj_dir.join(&rel).with_extension(tc.obj_ext());

      let mut arguments = tc.cc.clone();
      arguments.extend(tc.arch_flags());
      if let Some(std) = &compile.standard {
        arguments.extend(tc.std_args(std));
      }

      arguments.extend(tc.default_c_flags(&project.language));

      for dir in dep_include_dirs(project, root, cache_root) {
        arguments.push(tc.include_arg(&dir.display().to_string()));
      }

      arguments.extend(pkg_cflags.iter().filter(|s| !s.is_empty()).cloned());
      if let Some(include) = &compile.include {
        arguments
          .extend(include.list().into_iter().map(|dir| tc.include_arg(&dir)));
      }

      if shared_objs && let Some(pic) = tc.pic_flag() {
        arguments.push(pic);
      }

      arguments.extend(c_flags.iter().filter(|s| !s.is_empty()).cloned());
      if depfiles {
        arguments.extend(tc.depfile(&obj.display().to_string()).1);
      }
      arguments.extend(
        tc.compile_tail(&src.display().to_string(), &obj.display().to_string()),
      );

      let entry = CompileEntry {
        directory: root.display().to_string(),
        arguments,
        file: root.join(&src).display().to_string(),
      };

      Ok((obj, entry))
    })
    .collect()
}

/// Run `entries` in parallel, bounded by `threads`, under a progress bar. `env`
/// is the toolchain environment overlay (empty for gnu; the MSVC `vcvars` set
/// for `cl`).
pub fn compile(
  ui: &Ui,
  dir: &Path,
  entries: Vec<CompileEntry>,
  threads: usize,
  env: &[(OsString, OsString)],
) -> Result<()> {
  let pb = ui.bar(
    entries.len() as u64,
    format!("Compiling {} sources", entries.len()),
    "sources",
  );

  let pb2 = pb.clone();
  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(threads)
    .build()
    .into_diagnostic()?;

  let result = pool.install(|| {
    entries
      .par_iter()
      .progress_with(pb2)
      .map(|e| {
        let name = e.file.rsplit('/').next().unwrap_or(&e.file).to_string();
        let file_pb = ui.spinner(format!("Compiling {name}"));
        let result = ui.exec_env(dir, &e.arguments, env);
        if result.is_ok() {
          file_pb.finish_and_clear();
        } else {
          file_pb.finish_with_message(mark(
            Some(&StepStatus::Failure),
            &format!("Failed to compile {name}"),
          ));
        }
        result
      })
      .collect::<Result<()>>()
  });

  let status = if result.is_ok() {
    StepStatus::Success
  } else {
    StepStatus::Failure
  };

  ui.finish(&pb, &status, &format!("Compiled {} sources", entries.len()));

  result
}

/// Write `compile_commands.json` for clangd, resolving the active profile first
/// so the database matches a real profile build.
pub fn compile_commands(
  project: &Project,
  profile: Option<&str>,
  siblings: bool,
) -> Result<()> {
  let root = std::env::current_dir().into_diagnostic()?;

  let mut targets: Vec<(Project, PathBuf)> =
    vec![(project.clone(), root.clone())];
  if siblings && let Some(map) = &project.siblings {
    let mut sibs: Vec<_> = map.iter().collect();
    sibs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    for (_, sib) in sibs {
      let dir = root.join(&sib.path);
      targets.push((Project::from_file(dir.join("conjure.kdl"))?, dir));
    }
  }

  let mut entries = vec![];
  for (proj, dir) in targets {
    let resolved = profile.and_then(|name| proj.profile(name));
    let name = resolved.map_or("default", |(n, _)| n);
    let effective = proj.with_profile(resolved.map(|(_, p)| p));
    entries.extend(
      compile_entries(&dir, &root, &effective, name, vec![], false)?
        .into_iter()
        .map(|(_, e)| e),
    );

    // Test sources are excluded from the project's own entries, so add one
    // entry per test target using the same synthesis `conjure test` builds.
    if let Some(tests) = &effective.tests {
      let mut tests: Vec<_> = tests.iter().collect();
      tests.sort_by(|a, b| a.0.cmp(b.0));
      for (test_name, test) in tests {
        let test_proj = build::test_project(&proj, test_name, test, profile)?;
        let test_resolved = profile.and_then(|n| test_proj.profile(n));
        let test_out = test_resolved.map_or("default", |(n, _)| n);
        let test_eff = test_proj.with_profile(test_resolved.map(|(_, p)| p));
        entries.extend(
          compile_entries(&dir, &root, &test_eff, test_out, vec![], false)?
            .into_iter()
            .map(|(_, e)| e),
        );
      }
    }
  }
  fs::write(
    "compile_commands.json",
    serde_json::to_string_pretty(&entries).into_diagnostic()?,
  )
  .map_err(|source| WritePath {
    path: "compile_commands.json".into(),
    source,
  })?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn find_sources_filters_by_extension() {
    let dir =
      std::env::temp_dir().join(format!("conjure_fs_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("a.c"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();

    let mut out = vec![];
    find_sources(&dir, C_SRCS, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].ends_with("a.c"));

    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn collect_sources_accepts_file_roots() {
    let dir =
      std::env::temp_dir().join(format!("conjure_cs_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("a.c"), "").unwrap();
    std::fs::write(dir.join("b.c"), "").unwrap();

    // An exact file root is taken as-is, not recursed or filtered.
    assert_eq!(
      collect_sources(&dir, &["a.c".into()], C_SRCS).unwrap(),
      vec![dir.join("a.c")]
    );

    // A directory root still recurses.
    assert_eq!(
      collect_sources(&dir, &[".".into()], C_SRCS).unwrap().len(),
      2
    );

    let _ = std::fs::remove_dir_all(&dir);
  }
}
