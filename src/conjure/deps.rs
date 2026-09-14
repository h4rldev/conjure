//! External dependency resolution, fetching, and building.
//!
//! A dependency is either a local path or a remote git repo. Remote deps are
//! pinned in `conjure.lock`; an unlocked dep is cloned, resolved, and pinned
//! during the build automatically, so `conjure build` works without a prior
//! `conjure lock`. Fetching happens concurrently (network-bound) while building
//! stays serial in manifest order and drives each dep's own build system - or
//! conjure itself, for `build: conjure` deps.
//!
//! All paths are anchored per project scope so co-built siblings and the root
//! project share one cache under the invocation root without colliding. Cache
//! writes for dependencies are keyed by the resolved commit (remote) or a source
//! fingerprint (local), combined with the static/dynamic tag, so flips rebuild.
//!
//! Remote deps are always fetched (their trees supply headers), but they are
//! only *built* when the project actually links them. A static library archives
//! objects and never links, so its dependencies are fetched and then skipped.

/***********************************************************************/

use super::{
  build::{self, BuildCtx},
  dep_cache::DepCache,
  diag::ReadPath,
  fingerprint, git, link, lock,
  proj_parse::{
    BuildSystem, Dependency, Flags, Linkage, Project, ProjectType, Remote,
    Transport,
  },
  ui::{StepStatus, Ui},
};
use indicatif::ProgressBar;
use miette::{IntoDiagnostic, Result};
use rayon::prelude::*;
use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

/***********************************************************************/

/// The dep clone/cache home under a project root.
fn cache_home(root: &Path) -> PathBuf {
  root.join(".conjure").join("deps")
}

/// The clone dir for a scope. `.` (the invocation root project) maps to the
/// cache dir itself - never append a literal `/.`.
fn scope_dir(base: &Path, scope: &str) -> PathBuf {
  if scope == "." {
    base.to_path_buf()
  } else {
    base.join(scope)
  }
}

/// Per-project cache scope: the project dir relative to the invocation root.
/// `"."` for the top-level project, `"libs/sub1"` for a co-built sibling.
fn scope_of(root: &Path, dir: &Path) -> String {
  dir
    .strip_prefix(root)
    .ok()
    .filter(|p| !p.as_os_str().is_empty())
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or(".".to_string())
}

/// A dependency's source dir: local deps are relative to the project dir,
/// remote deps live under the scope's cache dir.
pub fn dep_dir(
  name: &str,
  dep: &Dependency,
  dir: &Path,
  root: &Path,
  scope: &str,
) -> PathBuf {
  match &dep.local {
    Some(p) => dir.join(p),
    None => scope_dir(&cache_home(root), scope).join(name),
  }
}

/// The clone URL for a remote dependency.
fn dep_url(dep: &Dependency, name: &str) -> Result<String> {
  let (host, path) = match dep.remote.as_ref() {
    Some(Remote::Codeberg(p)) => ("codeberg", p),
    Some(Remote::GitHub(p)) => ("github", p),
    Some(Remote::BitBucket(p)) => ("bitbucket", p),
    Some(Remote::Git(p)) => ("git", p),
    None => return Err(miette::miette!("Dependency `{name}` has no remote")),
  };

  let t = match dep.transport {
    Some(Transport::Ssh) | None => "ssh",
    Some(Transport::Https) => "https",
  };

  Ok(git::remote_url(host, path, t))
}

fn is_shared(name: &str) -> bool {
  if cfg!(target_os = "macos") {
    name.ends_with(".dylib") || name.ends_with(".so")
  } else if cfg!(target_os = "windows") {
    name.ends_with(".dll")
  } else {
    name.ends_with(".so")
      || name.split_once(".so.").is_some_and(|(_, ver)| {
        ver.chars().all(|c| c.is_ascii_digit() || c == '.')
      })
  }
}

fn is_static(name: &str) -> bool {
  // GNU `ar` archives are `.a` everywhere, including MinGW on Windows; MSVC
  // additionally uses `.lib`. External build systems may emit either.
  name.ends_with(".a") || (cfg!(target_env = "msvc") && name.ends_with(".lib"))
}

/// Record the first shared and first static library found anywhere under `dir`.
fn collect_libs(
  dir: &Path,
  shared: &mut Option<PathBuf>,
  statik: &mut Option<PathBuf>,
) -> Result<()> {
  for entry in fs::read_dir(dir).map_err(|source| ReadPath {
    path: dir.to_path_buf(),
    source,
  })? {
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

/// Pick the dep's library matching the linkage policy. Deterministic, where a
/// bare readdir walk would be arbitrary.
fn find_lib(dir: &Path, want_static: bool) -> Result<PathBuf> {
  let (mut shared, mut statik) = (None, None);
  collect_libs(dir, &mut shared, &mut statik)?;

  match (want_static, statik, shared) {
    (true, Some(p), _) => Ok(p),
    (true, None, Some(_)) => Err(miette::miette!(
      help = "Build the dependency to also emit a static (.a) archive, or link it dynamically",
      "Static build requested but `{}` only produced a shared library",
      dir.display()
    )),
    (false, _, Some(p)) => Ok(p),
    (false, Some(p), None) => Ok(p),
    _ => Err(miette::miette!("No library found in {}", dir.display())),
  }
}

/// Whether a token is link-relevant (`-l`/`-L`, raw `-Wl,`, `-pthread`, or a
/// library file path) rather than a compiler-only flag.
pub fn is_link_flag(tok: &str) -> bool {
  tok.starts_with("-l")
    || tok.starts_with("-L")
    || tok.starts_with("-Wl,")
    || tok == "-pthread"
    || [".a", ".so", ".dylib", ".lib", ".dll"]
      .iter()
      .any(|ext| tok.ends_with(ext))
}

/// Split pkg-config output into `(cflags, libs)`: `-l`/`-L`/`-Wl,`/`-pthread`
/// are link inputs, everything else is a compile flag.
fn split_cflags_and_libs(output: &str) -> (Vec<String>, Vec<String>) {
  let mut cflags = vec![];
  let mut libs = vec![];

  for tok in output.split_whitespace() {
    if is_link_flag(tok) {
      libs.push(tok.to_string());
    } else {
      cflags.push(tok.to_string());
    }
  }

  (cflags, libs)
}

/// How to build a dep: an explicit `build:` wins; otherwise a dep-root
/// `conjure.kdl` means build it with conjure, else the make default.
fn dep_build_system(dep: &Dependency, dep_dir: &Path) -> BuildSystem {
  match &dep.build {
    Some(b) => b.clone(),
    None if dep_dir.join("conjure.kdl").is_file() => BuildSystem::Conjure(None),
    None => BuildSystem::Make(None),
  }
}

/// Run an external build system for a dep, then return its produced library.
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
        help = "This is a bug: conjure deps are dispatched before `build_dep`",
        "Conjure dependency reached the external build path in `{}`",
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

/// How a remote dep is pinned for this build.
enum Pin {
  /// Already locked: check out this commit.
  Commit(String),
  /// Unlocked: resolve the manifest ref, then pin HEAD.
  Resolve,
}

/// A remote dep scheduled for the parallel fetch phase.
struct RemoteFetch<'a> {
  name: &'a str,
  dep: &'a Dependency,
  dep_dir: PathBuf,
  url: String,
  pin: Pin,
}

/// What a fetch produced for a previously-unlocked dep.
struct Resolved {
  r#ref: Option<String>,
  commit: String,
}

/// Fetch a dep and resolve the commit to pin it at: mirrors `conjure lock`,
/// checking out the manifest ref if present and then pinning HEAD.
fn resolve_remote(
  name: &str,
  dep: &Dependency,
  dep_dir: &Path,
  url: &str,
  bar: Option<&ProgressBar>,
) -> Result<Resolved> {
  git::ensure_cloned_at(dep_dir.parent().unwrap(), url, name, bar)?;
  if let Some(r) = &dep.r#ref {
    git::checkout(dep_dir, r)?;
  }

  let commit = git::resolve_head(dep_dir)?;
  let r#ref = git::head_ref(dep_dir)?;
  git::checkout(dep_dir, &commit)?;
  Ok(Resolved { r#ref, commit })
}

/// Fetch remote deps concurrently. Bars are pre-created in manifest order so
/// their display slots are stable, and completions are reported through
/// `Ui::println` so they line up with the running bars. Returns resolutions
/// aligned with `fetches` (`None` for already-locked entries).
fn fetch_remotes(
  ui: &Ui,
  fetches: &[RemoteFetch<'_>],
  threads: usize,
) -> Result<Vec<Option<Resolved>>> {
  if fetches.is_empty() {
    return Ok(vec![]);
  }

  let name_w = fetches.iter().map(|f| f.name.len()).max().unwrap_or(0);
  let bars: Vec<ProgressBar> = fetches
    .iter()
    .map(|f| ui.fetch_bar(f.name, name_w, "objects"))
    .collect();

  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(threads)
    .build()
    .into_diagnostic()?;

  let result: Result<Vec<Option<Resolved>>> = pool.install(|| {
    fetches
      .par_iter()
      .zip(bars.par_iter())
      .map(|(f, bar)| -> Result<Option<Resolved>> {
        let resolved = match &f.pin {
          Pin::Commit(commit) => {
            git::ensure_cloned_at(
              f.dep_dir.parent().unwrap(),
              &f.url,
              f.name,
              Some(bar),
            )?;
            git::checkout(&f.dep_dir, commit)?;
            None
          }
          Pin::Resolve => Some(resolve_remote(
            f.name,
            f.dep,
            &f.dep_dir,
            &f.url,
            Some(bar),
          )?),
        };

        bar.finish_and_clear();
        ui.println(Some(&StepStatus::Success), format!("Fetched {}", f.name))?;
        Ok(resolved)
      })
      .collect()
  });

  if result.is_err() {
    for (f, bar) in fetches.iter().zip(&bars) {
      if !bar.is_finished() {
        bar.finish_and_clear();
      }
      let _ = ui.println(
        Some(&StepStatus::Failure),
        format!("Failed to fetch {}", f.name),
      );
    }
  }

  result
}

/// Parent context a `build: conjure` dependency inherits and scopes against.
pub struct ConjureCtx<'a> {
  pub parent: &'a Project,
  pub dir: &'a Path,  // parent project dir
  pub root: &'a Path, // invocation root
  pub profile: Option<&'a str>,
  pub threads: usize,
}

/// Synthesize a manifestless dep project: the parent's compile settings
/// inherited, with `src` from `dep.src` (or the parent's roots) and includes
/// absolutized against the parent dir plus the dep's own includes.
pub fn manifestless_project(
  parent: &Project,
  dep: &Dependency,
  name: &str,
  parent_dir: &Path,
) -> Result<Project> {
  let parent_compile = parent.compile.as_ref().ok_or_else(|| {
    miette::miette!("Parent project has no compile section to inherit")
  })?;

  let mut compile = parent_compile.clone();
  compile.src = Some(Flags::Append(
    dep
      .src
      .clone()
      .unwrap_or_else(|| parent_compile.src_roots()),
  ));
  let mut include = vec![];
  if let Some(parent_inc) = &parent_compile.include {
    for inc in parent_inc.list() {
      include.push(parent_dir.join(inc).display().to_string());
    }
  }
  if let Some(dep_inc) = &dep.include {
    include.extend(dep_inc.iter().filter(|s| !s.is_empty()).cloned());
  }
  compile.include = (!include.is_empty()).then_some(Flags::Append(include));

  Ok(Project {
    name: name.to_string(),
    language: parent.language.clone(),
    ty: ProjectType::Library,
    link: if parent.want_static() {
      Linkage::Static
    } else {
      Linkage::Dynamic
    },
    compile: Some(compile),
    ..Default::default()
  })
}

pub fn local_dep_fingerprint(
  parent: &Project,
  parent_dir: &Path,
  profile_name: &str,
  name: &str,
  dep: &Dependency,
  cache: &mut fingerprint::FileCache,
) -> Result<String> {
  let dep_dir = parent_dir.join(dep.local.as_ref().expect("Local dep"));
  let kdl = dep_dir.join("conjure.kdl");

  if !matches!(dep_build_system(dep, &dep_dir), BuildSystem::Conjure(_)) {
    return fingerprint::dir_fingerprint(&dep_dir, cache);
  }

  let child = if kdl.is_file() {
    Project::from_file(&kdl)?
  } else {
    manifestless_project(parent, dep, name, parent_dir)?
  };

  let child_profile = child
    .profiles
    .as_ref()
    .and_then(|m| m.get(profile_name))
    .map(|p| (profile_name, p));

  let out_profile = child_profile.map_or("default", |(n, _)| n);
  let lock = lock::LockFile::load(dep_dir.join("conjure.lock"))?;
  let commits: Vec<&str> =
    lock.entries().values().map(|e| e.commit.as_str()).collect();

  fingerprint::fingerprint(
    &child.with_profile(child_profile.map(|(_, p)| p)),
    &dep_dir,
    out_profile,
    &commits,
    &[],
    cache,
  )
}

/// Build a dependency with conjure itself, in-process. Mode A: the dep root
/// carries its own `conjure.kdl`. Mode B (manifestless): the synthesized
/// [`manifestless_project`].
///
/// The dep's own profile matching the active name is applied when it has one,
/// otherwise it builds with no profile.
fn build_conjure_dep<'a>(
  ui: &Ui,
  ctx: &ConjureCtx<'a>,
  name: &str,
  dep: &Dependency,
) -> Result<PathBuf> {
  let dep_dir =
    dep_dir(name, dep, ctx.dir, ctx.root, &scope_of(ctx.root, ctx.dir));
  let kdl = dep_dir.join("conjure.kdl");

  let child = if kdl.is_file() {
    Project::from_file(&kdl)?
  } else {
    manifestless_project(ctx.parent, dep, name, ctx.dir)?
  };

  let child_profile = ctx.profile.and_then(|name| child.profile(name));
  let effective = child.with_profile(child_profile.map(|(_, p)| p));

  miette::ensure!(
    effective.is_library(),
    help = "Conjure dependencies must be libraries (`type library`)",
    "dependency `{name}` is a binary conjure project, which can't be linked"
  );
  miette::ensure!(
    !(ctx.parent.want_static() && !effective.want_static()),
    help = "Set the dependency to `link static`, or make this project dynamic",
    "dependency `{name}` is shared but this project links statically"
  );

  ui.println(
    Some(&StepStatus::Info),
    format!("Building {} with conjure", name),
  )?;

  let child_ctx = BuildCtx {
    project: &child,
    dir: dep_dir.to_path_buf(),
    root: ctx.root.to_path_buf(),
    profile: ctx.profile, // build_ctx resolves + prints the fallback note
    force: false,
    threads: Some(ctx.threads),
    record_state: true,
  };

  build::build_ctx(&child_ctx, ui)?;
  let out_profile = child_profile.map_or("default", |(n, _)| n);
  let scoped = link::output_scoped(&effective, &dep_dir, ctx.root);
  Ok(link::link_library_path(
    &effective,
    ctx.root,
    out_profile,
    scoped,
  ))
}

/// One dependency's resolved work item for a build.
struct DepJob<'a> {
  name: &'a str,
  dep: &'a Dependency,
  work: DepWork,
}

enum DepWork {
  /// Already built and cached: reuse the artifact.
  Cached(PathBuf),
  /// Source present locally; still needs building.
  Local { dep_dir: PathBuf, key: String },
  /// Remote, already fetched; still needs building.
  Remote { dep_dir: PathBuf, key: String },
}

/// Include dirs conjure hands the compiler for a project's deps: the dep's
/// listed includes, or the conventional root/include/build dirs when unspecified.
pub fn dep_include_dirs(
  project: &Project,
  dir: &Path,
  root: &Path,
) -> Vec<PathBuf> {
  let scope = scope_of(root, dir);
  let mut dirs = vec![];

  let mut deps: Vec<(&String, &Dependency)> = project
    .dependencies
    .as_ref()
    .map(|d| d.iter().collect())
    .unwrap_or_default();
  deps.sort_by(|a, b| a.0.cmp(b.0));
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

  dirs
}

/// Build every dependency and return their libraries in manifest order.
///
/// Three passes: fetch remote misses concurrently (auto-locking unlocked deps),
/// resolve each dep to a cache hit or a build job, then build serially and
/// record cache entries. Cache and lock writes stay on the main thread.
///
/// Returns an empty list for a static library, which needs dep headers but
/// never links dep libraries - the fetch still runs so those headers exist.
pub fn build_deps(
  ui: &Ui,
  proj: &Project,
  dir: &Path,
  root: &Path,
  profile: Option<&str>,
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

  let threads = compile.threads.unwrap_or_else(|| {
    std::thread::available_parallelism().map_or(1, |n| n.get())
  });

  let want_static = proj.want_static();
  let scope = scope_of(root, dir);
  let dep_base = cache_home(root);
  let key_tag = if want_static { "s" } else { "d" };
  let mut lock = lock::LockFile::load(dir.join("conjure.lock"))?;
  let mut cache = DepCache::load(root);
  let link_deps = !(proj.is_library() && want_static);

  // Pass 0: schedule fetches for remote deps that are neither cached nor locked.
  let mut fetches: Vec<RemoteFetch> = Vec::new();
  for (name, dep) in deps {
    if dep.local.is_some() {
      continue;
    }
    let dep_dir = scope_dir(&dep_base, &scope).join(name);
    let url = dep_url(dep, name)?;
    match lock.entries().get(name) {
      Some(locked) => {
        let key = format!("{}:{key_tag}", locked.commit);
        if cache.hit(&scope, name, &key).is_some() {
          continue; // cached; nothing to fetch
        }
        fetches.push(RemoteFetch {
          name,
          dep,
          dep_dir,
          url,
          pin: Pin::Commit(locked.commit.clone()),
        });
      }
      None => fetches.push(RemoteFetch {
        name,
        dep,
        dep_dir,
        url,
        pin: Pin::Resolve,
      }),
    }
  }

  let resolved = fetch_remotes(ui, &fetches, threads)?;
  let mut relocked = false;
  for (f, r) in fetches.iter().zip(&resolved) {
    if let Some(r) = r {
      lock.lock(
        f.name,
        f.dep.remote.clone(),
        f.dep.transport,
        r.r#ref.clone(),
        r.commit.clone(),
      );
      ui.println(Some(&StepStatus::Info), format!("Locked {}", f.name))?;
      relocked = true;
    }
  }
  if relocked {
    lock.save(dir.join("conjure.lock"))?;
  }

  if !link_deps {
    return Ok(vec![]);
  }

  // Pass 1: resolve each dep to a cache hit or a build job.
  let mut jobs: Vec<DepJob> = Vec::with_capacity(deps.len());
  for (name, dep) in deps {
    let work = match &dep.local {
      Some(p) => {
        let dep_dir = dir.join(p);
        let profile_name = profile.unwrap_or("default");
        let key = format!(
          "{}:{key_tag}",
          local_dep_fingerprint(
            proj,
            dir,
            profile_name,
            name,
            dep,
            &mut fingerprint::FileCache::default(),
          )?
        );
        match cache.hit(&scope, name, &key) {
          Some(lib) => {
            ui.println(
              Some(&StepStatus::Info),
              format!("Reusing cached for {name}"),
            )?;
            DepWork::Cached(lib)
          }
          None => DepWork::Local { dep_dir, key },
        }
      }
      None => {
        let locked = lock.entries().get(name).ok_or_else(|| {
          miette::miette!(
            "Dependency `{name}` is not locked; run `conjure lock`"
          )
        })?;

        let key = format!("{}:{key_tag}", locked.commit);
        match cache.hit(&scope, name, &key) {
          Some(lib) => {
            ui.println(
              Some(&StepStatus::Info),
              format!("Reusing cached for {name}"),
            )?;
            DepWork::Cached(lib)
          }
          None => DepWork::Remote {
            dep_dir: scope_dir(&dep_base, &scope).join(name),
            key,
          },
        }
      }
    };

    jobs.push(DepJob { name, dep, work });
  }

  // Pass 2: build in manifest order, recording cache entries.
  let dep_ctx = ConjureCtx {
    parent: proj,
    dir,
    root,
    profile,
    threads,
  };

  let mut libs = Vec::with_capacity(jobs.len());
  for job in &jobs {
    let lib = match &job.work {
      DepWork::Cached(lib) => lib.clone(),
      DepWork::Local { dep_dir, .. } | DepWork::Remote { dep_dir, .. } => {
        let system = dep_build_system(job.dep, dep_dir);
        if matches!(system, BuildSystem::Conjure(_)) {
          build_conjure_dep(ui, &dep_ctx, job.name, job.dep)?
        } else {
          build_dep(ui, dep_dir, &system, threads, want_static)?
        }
      }
    };

    if let DepWork::Local { key, .. } | DepWork::Remote { key, .. } = &job.work
    {
      cache.insert(&scope, job.name, key.clone(), lib.clone());
    }
    libs.push(lib);
  }

  cache.save(root);
  Ok(libs)
}

/// Resolve `pkg_config` entries for a project's deps into `(cflags, libs)`.
///
/// A static library never links dep libraries, so it resolves `--cflags` only
/// and returns empty libs.
pub fn resolve_pkg_config(
  project: &Project,
  dir: &Path,
  root: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
  let mut deps: Vec<(&String, &Dependency)> = project
    .dependencies
    .as_ref()
    .map(|d| d.iter().collect())
    .unwrap_or_default();

  deps.sort_by(|a, b| a.0.cmp(b.0));

  let scope = scope_of(root, dir);
  // A static library uses dep cflags but never links dep libs.
  let want_libs = !(project.is_library() && project.want_static());
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
    if want_libs {
      // `--static` only affects `--libs`, so it is requested together.
      cmd.arg("--libs");
      if project.want_static() {
        cmd.arg("--static");
      }
    }

    let out = cmd.args(pkgs).output().map_err(|source| {
      miette::miette!(
        help = "Install pkg-config, or remove the `pkg_config` field from dependency `{name}`",
        "Failed to run pkg-config for dependency `{name}` ({pkgs:?}): {source}"
      )
    })?;

    miette::ensure!(
      out.status.success(),
      help = "Make sure `{pkgs:?}` is installed and on PKG_CONFIG_PATH",
      "pkg-config failed for dependency `{name}` ({pkgs:?}): {}",
      String::from_utf8_lossy(&out.stderr),
    );

    let (cf, lb) = split_cflags_and_libs(&String::from_utf8_lossy(&out.stdout));
    cflags.extend(cf);
    libs.extend(lb);
  }

  Ok((cflags, libs))
}

#[cfg(test)]
mod tests {
  use super::{
    cache_home, dep_build_system, find_lib, is_shared, is_static,
    manifestless_project, scope_dir, scope_of, split_cflags_and_libs,
  };
  use crate::conjure::proj_parse::{
    BuildSystem, Compile, Dependency, Flags, Language, Project, ProjectType,
  };
  use std::path::Path;

  #[test]
  fn scope_dir_collapses_root_scope() {
    let base = Path::new("/p/.conjure/deps");

    assert_eq!(scope_dir(base, "."), base);
    assert_eq!(scope_dir(base, "libs/sub1"), base.join("libs/sub1"));
  }

  #[test]
  fn scope_of_root_is_dot() {
    assert_eq!(scope_of(Path::new("/r"), Path::new("/r")), ".");
    assert_eq!(scope_of(Path::new("/r"), Path::new("/r/libs/s")), "libs/s");
  }

  #[test]
  fn cache_home_is_root_conjure_deps() {
    assert_eq!(cache_home(Path::new("/r")), Path::new("/r/.conjure/deps"));
  }

  #[test]
  fn build_system_explicit_wins_over_manifest() {
    let dep = Dependency {
      local: Some(".".into()),
      build: Some(BuildSystem::CMake(None)),
      remote: None,
      transport: None,
      include: None,
      src: None,
      pkg_config: None,
      r#ref: None,
    };

    assert_eq!(
      dep_build_system(&dep, Path::new("/does/not/matter")),
      BuildSystem::CMake(None)
    );
  }

  #[test]
  fn build_system_defaults_to_make_without_manifest() {
    let dep = Dependency {
      local: Some(".".into()),
      build: None,
      remote: None,
      transport: None,
      include: None,
      src: None,
      pkg_config: None,
      r#ref: None,
    };

    assert_eq!(
      dep_build_system(&dep, Path::new("/definitely/not/here")),
      BuildSystem::Make(None)
    );
  }

  #[test]
  fn build_system_detects_conjure_manifest() {
    let dir =
      std::env::temp_dir().join(format!("conjure_dep_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("conjure.kdl"), "").unwrap();

    let dep = Dependency {
      local: Some(".".into()),
      build: None,
      remote: None,
      transport: None,
      include: None,
      src: None,
      pkg_config: None,
      r#ref: None,
    };

    assert_eq!(dep_build_system(&dep, &dir), BuildSystem::Conjure(None));
    let _ = std::fs::remove_dir_all(&dir);
  }

  fn dep(local: &str) -> Dependency {
    Dependency {
      local: Some(local.into()),
      build: None,
      remote: None,
      transport: None,
      include: None,
      src: None,
      pkg_config: None,
      r#ref: None,
    }
  }

  #[test]
  fn lib_name_classification() {
    #[cfg(target_os = "linux")]
    {
      assert!(is_shared("libx.so"));
      assert!(is_shared("libx.so.1"));
      assert!(!is_shared("libx.a"));
      assert!(!is_static("libx.so"));
    }
    #[cfg(target_os = "macos")]
    {
      assert!(is_shared("libx.dylib"));
      assert!(!is_shared("libx.a"));
    }
    #[cfg(all(windows, target_env = "msvc"))]
    {
      assert!(is_shared("x.dll"));
      assert!(is_static("x.lib"));
      assert!(is_static("x.a")); // GNU archive also valid on msvc
      assert!(!is_shared("x.lib"));
      assert!(!is_static("x.dll"));
    }
    #[cfg(all(windows, target_env = "gnu"))]
    {
      assert!(is_shared("x.dll"));
      assert!(is_static("libx.a"));
      assert!(!is_shared("libx.a"));
      assert!(!is_static("x.dll"));
    }
    assert!(!is_static("notes.txt"));
  }

  #[test]
  fn pkg_config_output_split() {
    let (cflags, libs) =
      split_cflags_and_libs("-I/inc -O2 -L/lib -lz -Wl,-rpath,/x -pthread");
    assert_eq!(cflags, vec!["-I/inc", "-O2"]);
    assert_eq!(libs, vec!["-L/lib", "-lz", "-Wl,-rpath,/x", "-pthread"]);
  }

  #[test]
  fn find_lib_handles_missing_and_static() {
    let dir =
      std::env::temp_dir().join(format!("conjure_lib_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let shared = match () {
      _ if cfg!(windows) => "x.dll",
      _ if cfg!(target_os = "macos") => "libx.dylib",
      _ => "libx.so",
    };
    let statik = if cfg!(all(windows, target_env = "msvc")) {
      "x.lib"
    } else {
      "libx.a"
    };

    assert!(find_lib(&dir, false).is_err());

    std::fs::write(dir.join(shared), "").unwrap();
    assert_eq!(find_lib(&dir, false).unwrap(), dir.join(shared));
    assert!(find_lib(&dir, true).is_err());

    std::fs::write(dir.join(statik), "").unwrap();
    assert_eq!(find_lib(&dir, true).unwrap(), dir.join(statik));

    let _ = std::fs::remove_dir_all(&dir);
  }
  #[test]
  fn manifestless_inherits_parent_and_overrides_src() {
    let parent = Project {
      name: "p".into(),
      language: Language::C,
      compile: Some(Compile {
        cc: Some("gcc".into()),
        standard: Some("c11".into()),
        include: Some(Flags::Append(vec!["inc".into()])),
        ..Default::default()
      }),
      ..Default::default()
    };

    let mut d = dep("../d");
    d.src = Some(vec!["source".into()]);
    d.include = Some(vec!["dinc".into()]);
    let child =
      manifestless_project(&parent, &d, "d", Path::new("/p")).unwrap();

    assert_eq!(child.ty, ProjectType::Library);
    assert_eq!(child.language, Language::C);
    let compile = child.compile.as_ref().unwrap();
    assert_eq!(compile.src_roots(), vec!["source"]); // dep.src wins
    assert_eq!(compile.standard.as_deref(), Some("c11")); // inherited
    let inc = compile.include.as_ref().unwrap().list();
    let expected_parent = Path::new("/p").join("inc").display().to_string();
    assert!(inc.contains(&expected_parent.to_string())); // parent include rooted
    assert!(inc.contains(&"dinc".to_string())); // dep include verbatim
    assert!(!child.want_static()); // follows the parent's dynamic default

    // No dep.src -> parent's src roots; no dep.include -> no include section.
    let bare =
      manifestless_project(&parent, &dep("../d"), "d", Path::new("/p"))
        .unwrap();
    let compile = bare.compile.as_ref().unwrap();
    assert_eq!(compile.src_roots(), vec!["src"]);
    assert!(compile.include.is_some()); // parent's include still carried
  }
}
