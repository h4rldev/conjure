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
  deps::dep_include_dirs,
  proj_parse::{Flags, Language, Profile, Project},
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

/// Recursively collect files under `dir` whose extension is in `exts`. An empty
/// `exts` collects every file (used to fingerprint include trees).
pub fn find_sources(
  dir: &Path,
  exts: &[&str],
  out: &mut Vec<PathBuf>,
) -> Result<()> {
  for entry in fs::read_dir(dir).into_diagnostic()? {
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
  let sources = collect_sources(root, &roots, exts)?;

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
  profile: Option<(&str, &Profile)>,
) -> Result<()> {
  let root = std::env::current_dir().into_diagnostic()?;
  let profile_name = profile.map_or("default", |(name, _)| name);
  let project = project.with_profile(profile.map(|(_, p)| p));
  let entries: Vec<_> =
    compile_entries(&root, &root, &project, profile_name, vec![])?
      .into_iter()
      .map(|(_, e)| e)
      .collect();
  fs::write(
    "compile_commands.json",
    serde_json::to_string_pretty(&entries).into_diagnostic()?,
  )
  .into_diagnostic()?;

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
