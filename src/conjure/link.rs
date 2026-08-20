use super::{
  proj_parse::{Compile, Project, ProjectType},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

pub struct LinkInputs<'a> {
  pub objects: &'a [PathBuf],
  pub libs: &'a [PathBuf],
  pub extra_libs: &'a [String],
  pub ld_flags: &'a [String],
  pub compile: &'a Compile,
}

fn link(
  ui: &Ui,
  objects: &[PathBuf],
  libs: &[PathBuf],
  extra_libs: &[String],
  compile: &Compile,
  ld_flags: &[String],
  out: &Path,
) -> Result<()> {
  let linker = compile
    .linker
    .as_deref()
    .filter(|s| !s.is_empty())
    .unwrap_or("cc");
  let mut argv: Vec<&str> = linker.split_whitespace().collect();

  argv.extend(objects.iter().filter_map(|p| p.to_str()));
  argv.extend(libs.iter().filter_map(|p| p.to_str()));
  argv.extend(extra_libs.iter().map(String::as_str));
  argv.extend(ld_flags.iter().map(String::as_str));
  argv.push("-o");
  argv.push(out.to_str().unwrap());

  ui.run_step(format!("Linking {}", out.display()), Path::new("."), &argv)
}

#[cfg(unix)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::unix::fs::symlink(target, link).into_diagnostic()
}

#[cfg(windows)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::windows::fs::symlink_file(target, link).into_diagnostic()
}

pub fn produce(
  ui: &Ui,
  project: &Project,
  profile_name: &str,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let output = project.output.as_ref();
  let symlink = output.and_then(|o| o.symlink_binaries).unwrap_or(false);

  match project.ty {
    ProjectType::BinaryDynamic | ProjectType::BinaryStatic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.bin.as_deref()).unwrap_or("bin")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;
      let bin = dir.join(&project.name);

      link(
        ui,
        inputs.objects,
        inputs.libs,
        inputs.extra_libs,
        inputs.compile,
        inputs.ld_flags,
        &bin,
      )?;

      if symlink {
        symlink_bin(&bin, Path::new(&project.name))?;
      }
      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", bin.display()),
      )?;
    }

    ProjectType::LibraryDynamic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;

      let lib = dir.join(format!("lib{}.so", project.name));
      let linker = inputs
        .compile
        .linker
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(inputs.compile.cc.as_deref())
        .unwrap_or("cc");

      let mut argv: Vec<&str> = linker.split_whitespace().collect();
      argv.extend(inputs.objects.iter().filter_map(|p| p.to_str()));
      argv.extend(inputs.ld_flags.iter().map(String::as_str));
      argv.push("-shared");
      argv.push("-o");
      argv.push(lib.to_str().unwrap());

      ui.run_step(format!("Linking {}", lib.display()), Path::new("."), &argv)?;
      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", lib.display()),
      )?;
    }

    ProjectType::LibraryStatic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;

      let lib = dir.join(format!("lib{}.a", project.name));

      let mut argv = vec!["ar", "rcs", lib.to_str().unwrap()];
      argv.extend(inputs.objects.iter().filter_map(|p| p.to_str()));

      ui.run_step(
        format!("Archiving {}", lib.display()),
        Path::new("."),
        &argv,
      )?;

      ui.run_step(
        format!("Indexing {}", lib.display()),
        Path::new("."),
        &["ranlib", lib.to_str().unwrap()],
      )?;

      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", lib.display()),
      )?;
    }
  }

  Ok(())
}
