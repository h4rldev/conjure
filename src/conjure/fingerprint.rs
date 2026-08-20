use super::{
  compile::{C_SRCS, CPP_SRCS, find_sources},
  proj_parse::{Language, Project},
};
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

fn file_fingerprint(paths: &[PathBuf]) -> Result<Vec<String>> {
  let mut out = paths
    .iter()
    .map(|p| {
      let md = fs::metadata(p).into_diagnostic()?;
      Ok(format!(
        "{}:{}:{:?}",
        p.display(),
        md.len(),
        md.modified().ok()
      ))
    })
    .collect::<Result<Vec<_>>>()?;
  out.sort();
  Ok(out)
}

pub fn fingerprint(project: &Project, profile_name: &str, dep_commits: &[&str]) -> Result<String> {
  let exts = match project.language {
    Language::C => C_SRCS,
    Language::Cpp => CPP_SRCS,
  };

  let mut items = vec![profile_name.to_string()];
  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("no compile section"))?;
  if let Some(cc) = &compile.cc {
    items.push(cc.clone());
  }
  if let Some(std) = &compile.standard {
    items.push(std.clone());
  }
  if let Some(inc) = &compile.include {
    items.extend(inc.iter().cloned());
  }
  if let Some(f) = &compile.c_flags {
    items.push(format!("{:?}", f));
  }
  if let Some(f) = &compile.ld_flags {
    items.push(format!("{:?}", f));
  }
  items.extend(dep_commits.iter().map(|s| s.to_string()));

  let mut sources = vec![];
  find_sources(Path::new("src"), exts, &mut sources)?;
  items.extend(file_fingerprint(&sources)?);

  if let Some(include) = &compile.include {
    for dir in include.iter().filter(|s| !s.is_empty()) {
      let mut files = vec![];
      find_sources(Path::new(dir), &[], &mut files)?;
      items.extend(file_fingerprint(&files)?);
    }
  }

  if let Some(deps) = &project.dependencies {
    for (name, dep) in deps {
      if let Some(path) = &dep.local {
        let mut files = vec![];
        find_sources(Path::new(path), &[], &mut files)?;
        items.push(format!("{name}:{}", file_fingerprint(&files)?.join(",")));
      }
    }
  }
  Ok(items.join("|"))
}
