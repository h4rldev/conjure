use super::{
  dep_cache::DepCache,
  git, lock,
  proj_parse::{BuildSystem, Project, Remote, Transport},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

pub fn dep_include_dirs(project: &Project) -> Vec<PathBuf> {
  let mut dirs = vec![];

  if let Some(deps) = &project.dependencies {
    for (name, dep) in deps {
      let base = match &dep.local {
        Some(p) => PathBuf::from(p),
        None => git::cache_dir().join(name),
      };

      match &dep.include {
        Some(dirs_list) => dirs.extend(dirs_list.iter().map(|d| base.join(d))),
        None => {
          dirs.push(base.clone());
          dirs.push(base.join("include"));
          dirs.push(base.join("build"));
        }
      }
    }
  }

  dirs
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

  walk(dir)?.ok_or_else(|| miette::miette!("No library found in {}", dir.display()))
}

fn split_cflags_and_libs(output: &str) -> (Vec<String>, Vec<String>) {
  let mut cflags = vec![];
  let mut libs = vec![];

  for tok in output.split_whitespace() {
    if tok.starts_with("-l")
      || tok.starts_with("-L")
      || tok.starts_with("-Wl,")
      || tok == "-pthread"
    {
      libs.push(tok.to_string());
    } else {
      cflags.push(tok.to_string());
    }
  }

  (cflags, libs)
}

fn build_dep(ui: &Ui, dir: &Path, system: &BuildSystem, threads: usize) -> Result<PathBuf> {
  let mut header: String = format!(
    "Building {} [{system}] using {threads} threads",
    dir.display()
  );

  let jobs = format!("-j{threads}");
  let mut commands: Vec<(PathBuf, Vec<String>)> = vec![];

  match system {
    BuildSystem::Make(t) => {
      let mut args = vec!["make".into(), jobs];
      if let Some(target) = t.as_deref() {
        args.push(target.into());
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
      }
      commands.push((dir.to_path_buf(), args));
    }

    BuildSystem::CMake(t) => {
      commands.push((
        dir.to_path_buf(),
        vec![
          "cmake".into(),
          "-S".into(),
          ".".into(),
          "-B".into(),
          "build".into(),
        ],
      ));

      let mut args = vec![
        "cmake".into(),
        "--build".into(),
        ".".into(),
        "--parallel".into(),
        threads.to_string(),
      ];
      if let Some(target) = t.as_deref() {
        args.push("--target".into());
        args.push(target.into());
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
      }

      commands.push((dir.join("build"), args));
    }

    BuildSystem::Autotools(t) => {
      commands.push((dir.to_path_buf(), vec!["./configure".into()]));
      let mut args = vec!["make".into(), jobs];
      if let Some(target) = t.as_deref() {
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
        args.push(target.into());
      }

      commands.push((dir.to_path_buf(), args));
    }

    BuildSystem::Meson(t) => {
      commands.push((
        dir.to_path_buf(),
        vec!["meson".into(), "setup".into(), "build".into(), ".".into()],
      ));
      let mut args = vec![
        "meson".into(),
        "compile".into(),
        "-j".into(),
        threads.to_string(),
      ];

      if let Some(target) = t.as_deref() {
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
        args.push(target.into());
      }

      commands.push((dir.join("build"), args));
    }

    BuildSystem::Ninja(t) => {
      let mut args = vec!["ninja".into(), jobs];
      if let Some(target) = t.as_deref() {
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
        args.push(target.into());
      }

      commands.push((dir.to_path_buf(), args));
    }

    BuildSystem::Xmake(t) => {
      let mut args = vec!["xmake".into(), jobs];
      if let Some(target) = t.as_deref() {
        header = format!(
          "Building {target} [{system}] [{}] using {threads} threads",
          dir.display()
        );
        args.push(target.into());
      }

      commands.push((dir.to_path_buf(), args));
    }

    BuildSystem::Just(t) => {
      let args = vec!["just".into(), t.into()];
      header = format!("Building {t} [{system}] [{}]", dir.display());
      commands.push((dir.to_path_buf(), args));
    }

    BuildSystem::Custom(cmd) => {
      header = format!("Building {} [{system}]", dir.display());
      commands.push((
        dir.to_path_buf(),
        vec!["sh".into(), "-c".into(), cmd.into()],
      ))
    }
  }

  let refs: Vec<(&Path, &[String])> = commands
    .iter()
    .map(|(d, a)| (d.as_path(), a.as_slice()))
    .collect();

  ui.run_dep(header, &refs)?;
  find_lib(dir)
}

pub fn build_deps(ui: &Ui, proj: &Project) -> Result<Vec<PathBuf>> {
  let deps = match &proj.dependencies {
    Some(d) if !d.is_empty() => d,
    _ => return Ok(vec![]),
  };

  let compile = proj.compile.as_ref().ok_or_else(|| {
    miette::miette!(
      help = "Double check if there's a compile section in the conjure.kdl",
      "No compile section in conjure.kdl"
    )
  })?;

  let threads = compile
    .threads
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));

  let lock = lock::LockFile::load("conjure.lock")?;
  let mut cache = DepCache::load();
  let mut libs = vec![];

  for (name, dep) in deps {
    let (dir, commit) = match &dep.local {
      Some(p) => (PathBuf::from(p), None),
      None => {
        let locked = lock.entries().get(name).ok_or_else(|| {
          miette::miette!("Dependency `{name}` is not locked; run `conjure lock`")
        })?;

        if let Some(lib) = cache.hit(name, &locked.commit) {
          ui.println(
            Some(&StepStatus::Info),
            format!("Reusing cached for {name}"),
          )?;
          libs.push(lib);
          continue;
        }

        let (host, path) = match &locked.remote {
          Some(Remote::Codeberg(p)) => ("codeberg", p),
          Some(Remote::GitHub(p)) => ("github", p),
          Some(Remote::BitBucket(p)) => ("bitbucket", p),
          Some(Remote::Git(p)) => ("git", p),
          None => return Err(miette::miette!("Dependency `{name}` has no remote")),
        };

        let t = match locked.transport {
          Some(Transport::Ssh) | None => "ssh",
          Some(Transport::Https) => "https",
        };

        let dir = git::ensure_cloned(Some(ui), &git::remote_url(host, path, t), name)?;
        git::checkout(&dir, &locked.commit)?;
        (dir, Some(locked.commit.clone()))
      }
    };

    let system = dep.build.clone().unwrap_or(BuildSystem::Make(None));
    let lib = build_dep(ui, &dir, &system, threads)?;
    if let Some(commit) = commit {
      cache.insert(name, commit, lib.clone());
    }
    libs.push(lib);
  }

  cache.save();
  Ok(libs)
}

pub fn resolve_pkg_config(project: &Project) -> Result<(Vec<String>, Vec<String>)> {
  let root = std::env::current_dir().into_diagnostic()?;

  let deps = match &project.dependencies {
    Some(d) => d,
    None => return Ok((vec![], vec![])),
  };

  let mut cflags = vec![];
  let mut libs = vec![];

  for (name, dep) in deps {
    let Some(pkgs) = &dep.pkg_config else {
      continue;
    };

    let dir = match &dep.local {
      Some(p) => PathBuf::from(p),
      None => git::cache_dir().join(name),
    };

    let build_dir = root.join(&dir).join("build");
    let pkg_path = std::env::var_os("PKG_CONFIG_PATH")
      .map(|p| format!("{}:{}", build_dir.display(), p.to_string_lossy()))
      .unwrap_or_else(|| build_dir.display().to_string());

    let out = Command::new("pkg-config")
      .env("PKG_CONFIG_PATH", pkg_path)
      .arg("--cflags")
      .arg("--libs")
      .args(pkgs)
      .output()
      .into_diagnostic()?;

    miette::ensure!(
      out.status.success(),
      "pkg-config failed for `{name}` ({pkgs:?}): {}",
      String::from_utf8_lossy(&out.stderr),
    );

    let (cf, lb) = split_cflags_and_libs(&String::from_utf8_lossy(&out.stdout));
    cflags.extend(cf);
    libs.extend(lb);
  }

  Ok((cflags, libs))
}
