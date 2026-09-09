use super::{
  build::{self, BuildCtx},
  dep_cache::DepCache,
  fingerprint, git, link, lock,
  proj_parse::{BuildSystem, Dependency, Profile, Project, ProjectType, Remote, Transport},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

fn cache_home(root: &Path) -> PathBuf {
  root.join(".conjure").join("deps")
}

/// Per-project cache scope: project dir relative to the invocation root.
/// `"."` for the top-level project, `"libs/sub1"` for a co-built sibling.
fn scope_of(root: &Path, dir: &Path) -> String {
  dir
    .strip_prefix(root)
    .ok()
    .filter(|p| !p.as_os_str().is_empty())
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or(".".to_string())
}

fn dep_dir(name: &str, dep: &Dependency, dir: &Path, root: &Path, scope: &str) -> PathBuf {
  match &dep.local {
    Some(p) => dir.join(p),
    None => cache_home(root).join(scope).join(name),
  }
}

pub fn dep_include_dirs(project: &Project, dir: &Path, root: &Path) -> Vec<PathBuf> {
  let scope = scope_of(root, dir);
  let mut dirs = vec![];

  if let Some(deps) = &project.dependencies {
    for (name, dep) in deps {
      let base = dep_dir(name, dep, dir, root, &scope);

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

fn is_shared(name: &str) -> bool {
  if cfg!(target_os = "macos") {
    name.ends_with(".dylib") || name.ends_with(".so")
  } else if cfg!(target_os = "windows") {
    name.ends_with(".dll")
  } else {
    name.ends_with(".so")
      || name
        .split_once(".so.")
        .is_some_and(|(_, ver)| ver.chars().all(|c| c.is_ascii_digit() || c == '.'))
  }
}

fn is_static(name: &str) -> bool {
  if cfg!(target_os = "windows") {
    name.ends_with(".lib")
  } else {
    name.ends_with(".a")
  }
}

fn collect_libs(
  dir: &Path,
  shared: &mut Option<PathBuf>,
  statik: &mut Option<PathBuf>,
) -> Result<()> {
  for entry in fs::read_dir(dir).into_diagnostic()? {
    let p = entry.into_diagnostic()?.path();
    if p.is_dir() {
      collect_libs(&p, shared, statik)?;
    } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
      if shared.is_none() && is_shared(name) {
        *shared = Some(p.clone());
      } else if statik.is_none() && is_static(name) {
        *statik = Some(p.clone());
      }
    }
  }
  Ok(())
}

/// Pick the dep's library matching the linkage policy. Deterministic, where
/// the old readdir-order walk was arbitrary.
fn find_lib(dir: &Path, want_static: bool) -> Result<PathBuf> {
  let (mut shared, mut statik) = (None, None);
  collect_libs(dir, &mut shared, &mut statik)?;

  match (want_static, statik, shared) {
    (true, Some(p), _) => Ok(p),
    (true, None, Some(_)) => Err(miette::miette!(
      help = "build the dependency to also emit a static (.a) archive, or link it dynamically",
      "static build requested but `{}` only produced a shared library",
      dir.display()
    )),
    (false, _, Some(p)) => Ok(p),
    (false, Some(p), None) => Ok(p),
    _ => Err(miette::miette!("No library found in {}", dir.display())),
  }
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

fn dep_build_system(dep: &Dependency, dep_dir: &Path) -> BuildSystem {
  match &dep.build {
    Some(b) => b.clone(),
    None if dep_dir.join("conjure.kdl").is_file() => BuildSystem::Conjure(None),
    None => BuildSystem::Make(None),
  }
}

fn build_dep(
  ui: &Ui,
  dir: &Path,
  system: &BuildSystem,
  threads: usize,
  want_static: bool,
) -> Result<PathBuf> {
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

    BuildSystem::Conjure(_) => {
      return Err(miette::miette!(
        help = "this is a bug: conjure deps are dispatched before `build_dep`",
        "conjure dependency reached the external build path in `{}`",
        dir.display()
      ));
    }
  }

  let refs: Vec<(&Path, &[String])> = commands
    .iter()
    .map(|(d, a)| (d.as_path(), a.as_slice()))
    .collect();

  ui.run_dep(header, &refs)?;
  find_lib(dir, want_static)
}

pub fn build_deps(
  ui: &Ui,
  proj: &Project,
  dir: &Path,
  root: &Path,
  profile: Option<(&str, &Profile)>,
  want_static: bool,
) -> Result<Vec<PathBuf>> {
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

  let scope = scope_of(root, dir);
  let dep_base = cache_home(root);
  let key_tag = if want_static { "s" } else { "d" };
  let dep_ctx = ConjureCtx {
    parent: proj,
    dir,
    root,
    profile,
  };

  let lock = lock::LockFile::load(dir.join("conjure.lock"))?;
  let mut cache = DepCache::load(root);
  let mut libs = vec![];

  for (name, dep) in deps {
    let (dep_dir, key) = match &dep.local {
      Some(p) => {
        let dep_dir = dir.join(p);
        let key = format!("{}:{key_tag}", fingerprint::dir_key(&dep_dir)?);
        if let Some(lib) = cache.hit(&scope, name, &key) {
          ui.println(
            Some(&StepStatus::Info),
            format!("Reusing cached for {name}"),
          )?;
          libs.push(lib);
          continue;
        }
        (dep_dir, Some(key))
      }
      None => {
        let locked = lock.entries().get(name).ok_or_else(|| {
          miette::miette!("Dependency `{name}` is not locked; run `conjure lock`")
        })?;

        let key = format!("{}:{key_tag}", locked.commit);
        if let Some(lib) = cache.hit(&scope, name, &key) {
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

        // project-scoped clone under the root cache
        let dep_dir = git::ensure_cloned_at(
          Some(ui),
          &dep_base.join(&scope),
          &git::remote_url(host, path, t),
          name,
        )?;
        git::checkout(&dep_dir, &locked.commit)?;
        (dep_dir, Some(key))
      }
    };
    let system = dep_build_system(dep, &dep_dir);
    let lib = if matches!(system, BuildSystem::Conjure(_)) {
      build_conjure_dep(ui, &dep_ctx, name, dep)?
    } else {
      build_dep(ui, &dep_dir, &system, threads, want_static)?
    };
    if let Some(k) = key {
      cache.insert(&scope, name, k, lib.clone());
    }
    libs.push(lib);
  }

  cache.save(root);
  Ok(libs)
}

pub fn resolve_pkg_config(
  project: &Project,
  dir: &Path,
  root: &Path,
  want_static: bool,
) -> Result<(Vec<String>, Vec<String>)> {
  let deps = match &project.dependencies {
    Some(d) => d,
    None => return Ok((vec![], vec![])),
  };

  let scope = scope_of(root, dir);
  let mut cflags = vec![];
  let mut libs = vec![];

  for (name, dep) in deps {
    let Some(pkgs) = &dep.pkg_config else {
      continue;
    };

    let dep_dir = dep_dir(name, dep, dir, root, &scope);
    let build_dir = dep_dir.join("build");
    let pkg_path = std::env::var_os("PKG_CONFIG_PATH")
      .map(|p| format!("{}:{}", build_dir.display(), p.to_string_lossy()))
      .unwrap_or_else(|| build_dir.display().to_string());

    let mut cmd = Command::new("pkg-config");
    cmd
      .current_dir(dir)
      .env("PKG_CONFIG_PATH", pkg_path)
      .arg("--cflags")
      .arg("--libs");
    if want_static {
      cmd.arg("--static");
    }
    let out = cmd.args(pkgs).output().into_diagnostic()?;

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

pub struct ConjureCtx<'a> {
  pub parent: &'a Project,
  pub dir: &'a Path,  // parent project dir
  pub root: &'a Path, // invocation root
  pub profile: Option<(&'a str, &'a Profile)>,
}

/// Build a dependency with conjure itself, in-process. Mode A: the dep root
/// carries its own conjure.kdl. Mode B (manifestless): compile settings are
/// inherited from the parent and only `src`/`include` are dep-specific.
fn build_conjure_dep<'a>(
  ui: &Ui,
  ctx: &ConjureCtx<'a>,
  name: &str,
  dep: &Dependency,
) -> Result<PathBuf> {
  let dep_dir = dep_dir(name, dep, ctx.dir, ctx.root, &scope_of(ctx.root, ctx.dir));
  let kdl = dep_dir.join("conjure.kdl");

  let (child, profile_name) = if kdl.is_file() {
    // Mode A: the dep has its own manifest.
    let child = Project::from_file(&kdl)?;
    miette::ensure!(
      matches!(
        child.ty,
        ProjectType::LibraryDynamic | ProjectType::LibraryStatic
      ),
      help = "conjure dependencies must be libraries (`type library dynamic` or `library static`)",
      "dependency `{name}` is a binary conjure project, which can't be linked"
    );
    miette::ensure!(
      !(want_static(ctx.parent) && matches!(child.ty, ProjectType::LibraryDynamic)),
      help = "set the dependency's conjure type to `library static`, or make this project dynamic",
      "dependency `{name}` is shared but this project links statically"
    );
    let profile_name = ctx.profile.map_or("default", |(n, _)| n);
    (child, profile_name)
  } else {
    // Mode B: manifestless — inherit parent compile settings.
    let child = manifestless_project(ctx.parent, dep, name, ctx.dir)?;
    let profile_name = ctx.profile.map_or("default", |(n, _)| n);
    (child, profile_name)
  };

  ui.println(
    Some(&StepStatus::Info),
    format!("Building {} with conjure", name),
  )?;

  let ctx = BuildCtx {
    project: &child,
    dir: dep_dir.to_path_buf(),
    root: ctx.root.to_path_buf(),
    profile: ctx.profile,
  };

  build::build_ctx(&ctx)?;
  Ok(link::library_path(&child, &ctx.root, profile_name))
}

fn want_static(project: &Project) -> bool {
  matches!(
    project.ty,
    ProjectType::BinaryStatic | ProjectType::LibraryStatic
  )
}

/// Synthesize a manifestless dep project: parent's compile settings inherited,
/// `src` roots = dep.src or the parent's compile src (default `["src"]`),
/// include dirs absolutized against the parent dir plus the dep's own includes.
fn manifestless_project(
  parent: &Project,
  dep: &Dependency,
  name: &str,
  parent_dir: &Path,
) -> Result<Project> {
  let parent_compile = parent
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("parent project has no compile section to inherit"))?;

  let mut compile = parent_compile.clone();
  compile.src = Some(
    dep
      .src
      .clone()
      .unwrap_or_else(|| parent_compile.src_roots()),
  );

  let mut include = vec![];
  if let Some(parent_inc) = &parent_compile.include {
    for inc in parent_inc.iter().filter(|s| !s.is_empty()) {
      include.push(parent_dir.join(inc).display().to_string());
    }
  }
  if let Some(dep_inc) = &dep.include {
    include.extend(dep_inc.iter().filter(|s| !s.is_empty()).cloned());
  }
  compile.include = (!include.is_empty()).then_some(include);

  Ok(Project {
    name: name.to_string(),
    language: parent.language.clone(),
    ty: if want_static(parent) {
      ProjectType::LibraryStatic
    } else {
      ProjectType::LibraryDynamic
    },
    compile: Some(compile),
    ..Default::default()
  })
}
