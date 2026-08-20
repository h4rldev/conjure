use super::{
  deps::dep_include_dirs,
  proj_parse::{Flags, Language, Profile, Project},
  ui::{StepStatus, Ui, mark},
};
use indicatif::ParallelProgressIterator;
use miette::{IntoDiagnostic, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::{
  fs,
  path::{Path, PathBuf},
};

pub const C_SRCS: &[&str] = &["c", "C"];
pub const CPP_SRCS: &[&str] = &["cpp", "cc", "cxx", "c++"];

#[derive(Debug, Serialize)]
pub struct CompileEntry {
  directory: String,
  arguments: Vec<String>,
  file: String,
}

pub fn find_sources(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) -> Result<()> {
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

pub fn flag_list(f: Option<&Flags>) -> Vec<String> {
  match f {
    None => vec![],
    Some(Flags::Append(v)) | Some(Flags::Replace(v)) => {
      v.iter().filter(|s| !s.is_empty()).cloned().collect()
    }
  }
}

pub fn merged_flags(base: Option<&Flags>, profile: Option<&Flags>) -> Vec<String> {
  let mut base = flag_list(base);

  match profile {
    None => base,
    Some(Flags::Append(p)) => {
      base.extend(p.iter().filter(|s| !s.is_empty()).cloned());
      base
    }
    Some(Flags::Replace(p)) => p.iter().filter(|s| !s.is_empty()).cloned().collect(),
  }
}

pub fn compile_commands(project: &Project, profile: Option<(&str, &Profile)>) -> Result<()> {
  let root = std::env::current_dir().into_diagnostic()?;
  let entries: Vec<_> = compile_entries(&root, project, profile, vec![])?
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

pub fn compile_entries(
  root: &Path,
  project: &Project,
  profile: Option<(&str, &Profile)>,
  pkg_cflags: Vec<String>,
) -> Result<Vec<(PathBuf, CompileEntry)>> {
  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("No compile section in conjure.kdl"))?;
  let profile_name = profile.map_or("default", |(name, _)| name);
  let profile_flags = profile.map(|(_, p)| p);

  let exts = match project.language {
    Language::C => C_SRCS,
    Language::Cpp => CPP_SRCS,
  };

  let mut sources = vec![];
  find_sources(Path::new("src"), exts, &mut sources)?;
  miette::ensure!(
    !sources.is_empty(),
    miette::miette!(
      help = "Make a main.c or main.cpp file in src/",
      "No source files found in src/"
    )
  );

  let c_flags = merged_flags(
    compile.c_flags.as_ref(),
    profile_flags.and_then(|p| p.c_flags.as_ref()),
  );

  let obj_dir = PathBuf::from(".conjure")
    .join("build")
    .join("obj")
    .join(profile_name);

  fs::create_dir_all(&obj_dir).into_diagnostic()?;

  let cc = compile
    .cc
    .as_deref()
    .filter(|s| !s.is_empty())
    .unwrap_or("cc");

  sources
    .into_iter()
    .map(|src| {
      let rel = src
        .strip_prefix("src")
        .unwrap_or(&src)
        .to_string_lossy()
        .replace(['/', '\\'], "_");
      let obj = obj_dir.join(format!("{rel}.o"));

      let mut arguments = vec![cc.to_string()];
      if let Some(std) = &compile.standard {
        arguments.push(format!("-std={std}"));
      }

      for dir in dep_include_dirs(project) {
        arguments.push(format!("-I{}", dir.display()));
      }

      arguments.extend(pkg_cflags.iter().filter(|s| !s.is_empty()).cloned());
      if let Some(include) = &compile.include {
        arguments.extend(
          include
            .iter()
            .filter(|s| !s.is_empty())
            .map(|dir| format!("-I{dir}")),
        );
      }

      arguments.extend(c_flags.iter().filter(|s| !s.is_empty()).cloned());
      arguments.extend([
        "-c".into(),
        src.display().to_string(),
        "-o".into(),
        obj.display().to_string(),
      ]);

      let entry = CompileEntry {
        directory: root.display().to_string(),
        arguments,
        file: root.join(&src).display().to_string(),
      };

      Ok((obj, entry))
    })
    .collect()
}

pub fn compile(ui: &Ui, entries: Vec<CompileEntry>, threads: usize) -> Result<()> {
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
        let result = ui.exec(Path::new("."), &e.arguments);
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
