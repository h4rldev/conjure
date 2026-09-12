//! The top-level build orchestration for one project.
//!
//! [`build_ctx`] runs the stages in order - fingerprint check, dependency build,
//! compile, link - then records the fingerprint so an unchanged tree short-
//! circuits next time. [`build`] adds the multi-project layer: it builds the
//! root project, then every sibling, all sharing one invocation root so their
//! dependency caches and artifact trees are scoped together.
//!
//! Sibling projects are separate `conjure.kdl` files built in the same
//! invocation; they are not the same as `build: conjure` dependencies, which are
//! built by `deps.rs`.

/***********************************************************************/

use super::{
  compile::{self, CompileEntry},
  deps, fingerprint, link, lock, proj_parse,
  proj_parse::{Flags, Profile, Project},
  toolchain,
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

/// Everything one project build needs: the project, where it lives, the shared
/// invocation root, the active profile, and whether to ignore the state file.
pub struct BuildCtx<'a> {
  pub project: &'a Project,
  pub dir: PathBuf,
  pub root: PathBuf,
  /// Selected profile *name*; resolved against `project`'s own profiles. A name
  /// the project does not define falls back to default (see `build_ctx`).
  pub profile: Option<&'a str>,
  pub force: bool,
}

impl<'a> BuildCtx<'a> {
  /// The named profile if this project defines it, else `None` (default build).
  fn resolve(&self) -> Option<(&'a str, &'a Profile)> {
    self.profile.and_then(|name| self.project.profile(name))
  }

  /// The profile actually built: the named one if defined, else `"default"`.
  pub fn profile_name(&self) -> &str {
    self.resolve().map_or("default", |(name, _)| name)
  }
}

/// The path a project's build produces, used to invalidate the "nothing to do"
/// shortcut when the artifact has been deleted.
fn built_artifact(
  project: &Project,
  dir: &Path,
  root: &Path,
  profile_name: &str,
) -> PathBuf {
  let scoped = link::output_scoped(project, dir, root);
  if project.is_library() {
    link::library_path(project, root, profile_name, scoped)
  } else {
    link::binary_path(project, root, profile_name, scoped)
  }
}

/// Build one project: skip if current, else build deps, compile, link, and
/// record the fingerprint.
pub fn build_ctx(ctx: &BuildCtx, ui: &Ui) -> Result<()> {
  if let Some(name) = ctx.profile
    && ctx.resolve().is_none()
  {
    ui.println(
      Some(&StepStatus::Info),
      format!(
        "{}: no `{name}` profile; building default",
        ctx.project.name
      ),
    )?;
  }
  let profile_name = ctx.profile_name();
  let project = ctx.project.with_profile(ctx.resolve().map(|(_, p)| p));

  let lock = lock::LockFile::load(ctx.dir.join("conjure.lock"))?;
  let dep_commits: Vec<String> =
    lock.entries().values().map(|e| e.commit.clone()).collect();
  let mut file_cache = fingerprint::FileCache::load(&ctx.dir);
  let fp = fingerprint::fingerprint(
    &project,
    &ctx.dir,
    profile_name,
    &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
    &mut file_cache,
  )?;

  // Saved even on a no-op build, so recorded mtimes advance and the next run
  // does not re-read files whose mtime changed without a content change.
  file_cache.save(&ctx.dir);

  let state_file = ctx.dir.join(".conjure/build/state.kdl");
  let up_to_date = !ctx.force
    && fs::read_to_string(&state_file).is_ok_and(|s| s == fp)
    && built_artifact(&project, &ctx.dir, &ctx.root, profile_name).exists();
  if up_to_date {
    ui.println(Some(&StepStatus::Info), "Nothing to do")?;
    return Ok(());
  }

  let dep_libs =
    deps::build_deps(ui, &project, &ctx.dir, &ctx.root, ctx.profile)?;

  let (pkg_cflags, pkg_libs) =
    deps::resolve_pkg_config(&project, &ctx.dir, &ctx.root)?;

  let (objects, entries): (Vec<PathBuf>, Vec<CompileEntry>) =
    compile::compile_entries(
      &ctx.dir,
      &ctx.root,
      &project,
      profile_name,
      pkg_cflags,
    )?
    .into_iter()
    .unzip();

  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("No compile section in conjure.kdl"))?;

  let ld_flags = compile
    .ld_flags
    .as_ref()
    .map(Flags::list)
    .unwrap_or_default();

  let threads = compile.threads.unwrap_or_else(|| {
    std::thread::available_parallelism().map_or(1, |n| n.get())
  });

  let build_env = toolchain::resolve(compile, &project.language).env();
  compile::compile(ui, &ctx.dir, entries, threads, build_env)?;

  let inputs = link::LinkInputs {
    objects: &objects,
    libs: &dep_libs,
    extra_libs: &pkg_libs,
    ld_flags: &ld_flags,
    compile,
  };

  link::produce(ui, &project, &ctx.dir, &ctx.root, profile_name, &inputs)?;

  let _ = fs::create_dir_all(state_file.parent().unwrap());
  let _ = fs::write(state_file, fp);
  Ok(())
}

/// Build a project and all of its siblings under the current directory.
pub fn build(
  project: &Project,
  profile: Option<&str>,
  force: bool,
  siblings: bool,
) -> Result<()> {
  let cwd = std::env::current_dir().into_diagnostic()?;
  let ui = Ui::new();
  build_ctx(
    &BuildCtx {
      project,
      dir: cwd.clone(),
      root: cwd.clone(),
      profile,
      force,
    },
    &ui,
  )?;

  if !siblings {
    return Ok(());
  }

  if let Some(map) = &project.siblings {
    // HashMap iteration is nondeterministic; sort so multi-sibling runs are.
    let mut sibs: Vec<_> = map.iter().collect();
    sibs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    for (_, sib) in sibs {
      let sib_dir = cwd.join(&sib.path);
      let sib_project =
        proj_parse::Project::from_file(sib_dir.join("conjure.kdl"))?;
      build_ctx(
        &BuildCtx {
          project: &sib_project,
          dir: sib_dir,
          root: cwd.clone(),
          profile, // same *name*; build_ctx resolves it per sibling
          force,
        },
        &ui,
      )?;
    }
  }
  Ok(())
}
