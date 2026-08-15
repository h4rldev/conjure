use super::{
  git, lock,
  proj_parse::{
    BuildSystem, Compile, Flags, Language, Profile, Project, ProjectType, Remote, Transport,
  },
};
use miette::{IntoDiagnostic, Result};
use rayon::prelude::*;
use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

fn run(dir: &Path, args: &[&str]) -> Result<()> {
  let status = Command::new(args[0])
    .args(&args[1..])
    .current_dir(dir)
    .status()
    .into_diagnostic()?;

  miette::ensure!(
    status.success(),
    "`{}` failed in {}",
    args.join(" "),
    dir.display()
  );
  Ok(())
}

fn flag_list(f: Option<&Flags>) -> Vec<String> {
  match f {
    None => vec![],
    Some(Flags::Append(v)) | Some(Flags::Replace(v)) => {
      v.iter().filter(|s| !s.is_empty()).cloned().collect()
    }
  }
}

fn merged_flags(base: Option<&Flags>, profile: Option<&Flags>) -> Vec<String> {
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

fn find_sources(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> Result<()> {
  for entry in fs::read_dir(dir).into_diagnostic()? {
    let p = entry.into_diagnostic()?.path();
    if p.is_dir() {
      find_sources(&p, ext, out)?;
    } else if p.extension().is_some_and(|e| e == ext) {
      out.push(p);
    }
  }

  Ok(())
}

fn is_lib(name: &str) -> bool {
  let dynamic = if cfg!(target_os = "macos") {
    name.ends_with(".dylib") || name.ends_with(".so")
  } else if cfg!(target_os = "windows") {
    name.ends_with(".dll")
  } else {
    name.ends_with(".so")
      || name
        .split_once(".so.")
        .is_some_and(|(_, ver)| ver.chars().all(|c| c.is_ascii_digit() || c == '.'))
  };

  let static_ext = if cfg!(target_os = "windows") {
    ".lib"
  } else {
    ".a"
  };
  dynamic || name.ends_with(static_ext)
}

fn find_lib(dir: &Path) -> Result<PathBuf> {
  fn walk(dir: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir).into_diagnostic()? {
      let p = entry.into_diagnostic()?.path();
      if p.is_dir() {
        if let Some(f) = walk(&p)? {
          return Ok(Some(f));
        }
      } else if p.file_name().is_some_and(|n| is_lib(&n.to_string_lossy())) {
        return Ok(Some(p));
      }
    }
    Ok(None)
  }

  walk(dir)?.ok_or_else(|| miette::miette!("no library found in {}", dir.display()))
}

fn compile_one(obj_dir: &Path, src: &Path, compile: &Compile, flags: &[String]) -> Result<PathBuf> {
  let cc = compile.cc.as_deref().unwrap_or("cc");
  let rel = src
    .strip_prefix("src")
    .unwrap_or(src)
    .to_string_lossy()
    .replace(['/', '\\'], "_");
  let obj = obj_dir.join(format!("{rel}.o"));
  let mut cmd = Command::new(cc);

  if let Some(std) = &compile.standard {
    cmd.arg(format!("-std={std}"));
  }

  cmd.args(flags).arg("-c").arg(src).arg("-o").arg(&obj);
  let status = cmd.status().into_diagnostic()?;
  miette::ensure!(status.success(), "failed to compile {}", src.display());
  Ok(obj)
}

fn build_dep(dir: &Path, system: &BuildSystem) -> Result<PathBuf> {
  match system {
    BuildSystem::Make => run(dir, &["make"])?,
    BuildSystem::CMake => {
      run(dir, &["cmake", "-S", ".", "-B", "build"])?;
      run(&dir.join("build"), &["cmake", "--build", "."])?;
    }
    BuildSystem::Autotools => {
      run(dir, &["./configure"])?;
      run(dir, &["make"])?;
    }
    BuildSystem::Meson => {
      run(dir, &["meson", "setup", "build", "."])?;
      run(&dir.join("build"), &["meson", "compile"])?;
    }
    BuildSystem::Ninja => run(dir, &["ninja"])?,
    BuildSystem::Xmake => run(dir, &["xmake"])?,
    BuildSystem::Custom(cmd) => {
      let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .status()
        .into_diagnostic()?;
      miette::ensure!(status.success(), "custom build `{cmd}` failed");
    }
  }

  find_lib(dir)
}

fn build_deps(proj: &Project) -> Result<Vec<PathBuf>> {
  let deps = match &proj.dependencies {
    Some(d) if !d.is_empty() => d,
    _ => return Ok(vec![]),
  };

  let lock = lock::LockFile::load("conjure.lock")?;
  let mut libs = vec![];
  for (name, dep) in deps {
    let dir = match &dep.local {
      Some(p) => PathBuf::from(p),
      None => {
        let locked = lock.entries().get(name).ok_or_else(|| {
          miette::miette!("dependency `{name}` is not locked; run `conjure lock`")
        })?;

        let (host, path) = match &locked.remote {
          Some(Remote::Codeberg(p)) => ("codeberg", p),
          Some(Remote::GitHub(p)) => ("github", p),
          Some(Remote::BitBucket(p)) => ("bitbucket", p),
          Some(Remote::Git(p)) => ("git", p),
          None => return Err(miette::miette!("dependency `{name}` has no remote")),
        };

        let t = match locked.transport {
          Some(Transport::Ssh) | None => "ssh",
          Some(Transport::Https) => "https",
        };

        let dir = git::ensure_cloned(&git::remote_url(host, path, t), name)?;
        git::checkout(&dir, &locked.commit)?;
        dir
      }
    };

    let system = dep.build.clone().unwrap_or(BuildSystem::Make);
    println!("building dependency `{name}` with `{system}`");
    libs.push(build_dep(&dir, &system)?);
  }

  Ok(libs)
}

fn link(
  objects: &[PathBuf],
  libs: &[PathBuf],
  compile: &Compile,
  ld_flags: &[String],
  out: &Path,
) -> Result<()> {
  let linker = compile.linker.as_deref().unwrap_or("cc");
  let status = Command::new(linker)
    .args(objects)
    .args(libs)
    .args(ld_flags)
    .arg("-o")
    .arg(out)
    .status()
    .into_diagnostic()?;

  miette::ensure!(status.success(), "failed to link");
  Ok(())
}

#[cfg(unix)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::unix::fs::symlink(target, link).into_diagnostic()
}

#[cfg(windows)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::windows::fs::symlink_file(target, link).into_diagnostic()
}

pub fn build(project: &Project, profile: Option<(&str, &Profile)>) -> Result<()> {
  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("no compile section in conjure.kdl"))?;
  let profile_name = profile.map_or("default", |(name, _)| name);
  let profile_flags = profile.map(|(_, p)| p);

  let ext = match project.language {
    Language::C => "c",
    Language::Cpp => "cpp",
  };

  let mut sources = vec![];
  find_sources(Path::new("src"), ext, &mut sources)?;
  miette::ensure!(!sources.is_empty(), "no source files found in src/");

  let c_flags = merged_flags(
    compile.c_flags.as_ref(),
    profile_flags.and_then(|p| p.c_flags.as_ref()),
  );
  let ld_flags = merged_flags(
    compile.ld_flags.as_ref(),
    profile_flags.and_then(|p| p.ld_flags.as_ref()),
  );

  let obj_dir = PathBuf::from(".conjure").join("build").join("obj");
  fs::create_dir_all(&obj_dir).into_diagnostic()?;

  let threads = compile
    .threads
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));

  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(threads)
    .build()
    .into_diagnostic()?;

  let objects = pool.install(|| {
    sources
      .par_iter()
      .map(|s| compile_one(&obj_dir, s, compile, &c_flags))
      .collect::<Result<Vec<_>>>()
  })?;

  let dep_libs = build_deps(project)?;
  let output = project.output.as_ref();
  let symlink = output.and_then(|o| o.symlink_binaries).unwrap_or(false);

  match project.ty {
    ProjectType::BinaryStatic | ProjectType::BinaryDynamic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.bin.as_deref()).unwrap_or("bin")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;
      let bin = dir.join(&project.name);

      link(&objects, &dep_libs, compile, &ld_flags, &bin)?;
      if symlink {
        symlink_bin(&bin, Path::new(&project.name))?;
      }

      println!("built `{}`", bin.display());
    }
    ProjectType::LibraryDynamic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;
      let lib = dir.join(&project.name);
      let linker = compile
        .linker
        .as_deref()
        .or(compile.cc.as_deref())
        .unwrap_or("cc");

      let status = Command::new(linker)
        .args(&objects)
        .args(&ld_flags)
        .arg("-shared")
        .arg("-o")
        .arg(&lib)
        .status()
        .into_diagnostic()?;

      miette::ensure!(status.success(), "failed to link shared library");
      println!("built `{}`", lib.display());
    }
    ProjectType::LibraryStatic => {
      let dir =
        PathBuf::from(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib")).join(profile_name);

      fs::create_dir_all(&dir).into_diagnostic()?;
      let lib = dir.join(format!("lib{}.a", project.name));
      let status = Command::new("ar")
        .arg("rcs")
        .arg(&lib)
        .args(&objects)
        .status()
        .into_diagnostic()?;

      miette::ensure!(status.success(), "failed to create static library");

      let status = Command::new("ranlib")
        .arg(&lib)
        .status()
        .into_diagnostic()?;

      miette::ensure!(status.success(), "failed to ranlib static library");

      println!("built `{}`", lib.display());
    }
  }

  Ok(())
}
