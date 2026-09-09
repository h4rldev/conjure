use super::{
  proj_parse::{Compile, Project, ProjectType},
  toolchain::{self, Toolchain},
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

/// Destination for one link step.
///
/// `dir` is the cwd the linker runs in, so anything in the produced argv that
/// isn't absolute resolves against it. `root` is where artifacts are scoped
/// (`bin|lib/<project>/<profile>`); the top-level build keeps it == `dir`,
/// siblings get the parent as `root`. `out` may be absolute or relative to
/// `dir`. `symlink` only applies to binaries: after a successful link, drop a
/// convenience `<root>/<project.name>` link pointing at `out`.
pub struct LinkTarget<'a> {
  pub dir: &'a Path,
  pub root: &'a Path,
  pub out: &'a Path,
  pub symlink: bool,
}

#[cfg(target_os = "macos")]
fn shared_ext() -> &'static str {
  "dylib"
}
#[cfg(not(target_os = "macos"))]
fn shared_ext() -> &'static str {
  "so"
}
/// The exact library path `produce` writes for a library project, honoring
/// its `output.lib` override. conjure-built deps return this instead of
/// walking the dep dir for a library.
pub fn library_path(project: &Project, root: &Path, profile_name: &str) -> PathBuf {
  let output = project.output.as_ref();
  let out_dir = root
    .join(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib"))
    .join(&project.name)
    .join(profile_name);
  match project.ty {
    ProjectType::LibraryDynamic => out_dir.join(format!("lib{}.{}", project.name, shared_ext())),
    ProjectType::LibraryStatic => out_dir.join(format!("lib{}.a", project.name)),
    ProjectType::BinaryStatic | ProjectType::BinaryDynamic => {
      unreachable!("library_path called on a binary project")
    }
  }
}

fn binary_argv(
  tc: &Toolchain,
  objects: &[PathBuf],
  libs: &[PathBuf],
  extra_libs: &[String],
  flags: &[String],
  out: &str,
  is_static: bool,
) -> Vec<String> {
  let mut argv = tc.linker.clone();
  if is_static
    && !flags.iter().any(|f| f == "-static")
    && let Some(sf) = tc.static_flag()
  {
    argv.push(sf);
  }
  argv.extend(objects.iter().filter_map(|p| p.to_str().map(String::from)));
  argv.extend(libs.iter().filter_map(|p| p.to_str().map(String::from)));
  argv.extend(extra_libs.iter().cloned());
  argv.extend(flags.iter().cloned());
  argv.push("-o".into());
  argv.push(out.into());
  argv
}

fn shared_argv(
  tc: &Toolchain,
  objects: &[PathBuf],
  extra_libs: &[String],
  flags: &[String],
  out: &str,
) -> Vec<String> {
  let mut argv = tc.linker.clone();
  argv.extend(objects.iter().filter_map(|p| p.to_str().map(String::from)));
  argv.extend(extra_libs.iter().cloned());
  argv.extend(flags.iter().cloned());
  argv.push(if cfg!(target_os = "macos") {
    "-dynamiclib".into()
  } else {
    "-shared".into()
  });
  argv.push("-o".into());
  argv.push(out.into());
  argv
}
fn is_static_flag(f: &str) -> bool {
  matches!(f, "-static" | "-Wl,-static")
}

fn link_binary(
  ui: &Ui,
  tc: &Toolchain,
  project: &Project,
  target: &LinkTarget,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let is_static = matches!(project.ty, ProjectType::BinaryStatic);

  let mut flags: Vec<String> = inputs.ld_flags.to_vec();
  if !is_static {
    let removed: Vec<String> = flags
      .iter()
      .filter(|f| is_static_flag(f))
      .cloned()
      .collect();
    flags.retain(|f| !is_static_flag(f));
    if !removed.is_empty() {
      ui.println(
        Some(&StepStatus::Info),
        format!(
          "ignored static link flag{} for dynamic binary `{}`: {}",
          if removed.len() > 1 { "s" } else { "" },
          project.name,
          removed.join(" ")
        ),
      )?;
    }
  }

  let out = target.out.strip_prefix(target.dir).unwrap_or(target.out);
  let argv = binary_argv(
    tc,
    inputs.objects,
    inputs.libs,
    inputs.extra_libs,
    &flags,
    out.to_str().unwrap(),
    is_static,
  );

  ui.run_step(
    format!("Linking {}", target.out.display()),
    target.dir,
    &argv,
  )?;

  if target.symlink {
    symlink_bin(target.out, &target.root.join(&project.name))?;
  }

  ui.println(
    Some(&StepStatus::Success),
    format!("Built {}", target.out.display()),
  )
}

fn link_shared(
  ui: &Ui,
  tc: &Toolchain,
  target: &LinkTarget,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let argv = shared_argv(
    tc,
    inputs.objects,
    inputs.extra_libs,
    inputs.ld_flags,
    target
      .out
      .strip_prefix(target.dir)
      .unwrap_or(target.out)
      .to_str()
      .unwrap(),
  );

  ui.run_step(
    format!("Linking {}", target.out.display()),
    target.dir,
    &argv,
  )?;

  ui.println(
    Some(&StepStatus::Success),
    format!("Built {}", target.out.display()),
  )
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
  dir: &Path,
  root: &Path,
  profile_name: &str,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let output = project.output.as_ref();
  let symlink = output.and_then(|o| o.symlink_binaries).unwrap_or(false);
  let tc = toolchain::resolve(inputs.compile);

  let bin_dir = |default: &str| {
    root
      .join(output.and_then(|o| o.bin.as_deref()).unwrap_or(default))
      .join(&project.name)
      .join(profile_name)
  };

  match project.ty {
    ProjectType::BinaryStatic | ProjectType::BinaryDynamic => {
      let out_dir = bin_dir("bin");
      fs::create_dir_all(&out_dir).into_diagnostic()?;
      let bin = out_dir.join(&project.name);
      link_binary(
        ui,
        &tc,
        project,
        &LinkTarget {
          dir,
          root,
          out: &bin,
          symlink,
        },
        inputs,
      )?;
    }

    ProjectType::LibraryDynamic => {
      let out_dir = root
        .join(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib"))
        .join(&project.name)
        .join(profile_name);
      fs::create_dir_all(&out_dir).into_diagnostic()?;
      let lib = library_path(project, root, profile_name);
      link_shared(
        ui,
        &tc,
        &LinkTarget {
          dir,
          root,
          out: &lib,
          symlink: false,
        },
        inputs,
      )?;
    }

    ProjectType::LibraryStatic => {
      let out_dir = root
        .join(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib"))
        .join(&project.name)
        .join(profile_name);

      fs::create_dir_all(&out_dir).into_diagnostic()?;

      let lib = library_path(project, root, profile_name);
      let rel_lib = lib.strip_prefix(dir).unwrap_or(&lib);

      let mut argv = vec!["ar", "rcs", rel_lib.to_str().unwrap()];
      argv.extend(inputs.objects.iter().filter_map(|p| p.to_str()));

      ui.run_step(format!("Archiving {}", lib.display()), dir, &argv)?;
      ui.run_step(
        format!("Indexing {}", lib.display()),
        dir,
        &["ranlib", rel_lib.to_str().unwrap()],
      )?;

      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", lib.display()),
      )?;
    }
  }

  Ok(())
}
