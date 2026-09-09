use super::{
  compile::{self, CompileEntry},
  deps, fingerprint, link, lock, proj_parse,
  proj_parse::{Profile, Project},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{fs, path::PathBuf};

pub struct BuildCtx<'a> {
  pub project: &'a Project,
  pub dir: PathBuf,
  pub root: PathBuf,
  pub profile: Option<(&'a str, &'a Profile)>,
}

impl<'a> BuildCtx<'a> {
  pub fn profile_name(&self) -> &str {
    self.profile.map_or("default", |(name, _)| name)
  }
}

pub fn build(project: &Project, profile: Option<(&str, &Profile)>) -> Result<()> {
  let cwd = std::env::current_dir().into_diagnostic()?;
  let ctx = BuildCtx {
    project,
    dir: cwd.clone(),
    root: cwd.clone(),
    profile,
  };
  build_ctx(&ctx)?;

  if let Some(siblings) = &project.siblings {
    for sib in siblings.values() {
      let sib_dir = cwd.join(&sib.path);
      let sib_project = proj_parse::Project::from_file(sib_dir.join("conjure.kdl"))?;
      let sib_ctx = BuildCtx {
        project: &sib_project,
        dir: sib_dir,
        root: cwd.clone(),
        profile,
      };
      build_ctx(&sib_ctx)?;
    }
  }
  Ok(())
}

pub fn build_ctx(ctx: &BuildCtx) -> Result<()> {
  let ui = Ui::new();
  let project = ctx.project;
  let profile = ctx.profile;
  let profile_name = ctx.profile_name();

  let lock = lock::LockFile::load(ctx.dir.join("conjure.lock"))?;
  let dep_commits: Vec<String> = lock.entries().values().map(|e| e.commit.clone()).collect();
  let fp = fingerprint::fingerprint(
    project,
    &ctx.dir,
    profile_name,
    &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
  )?;

  let state_file = ctx.dir.join(".conjure/build/state.kdl");
  if fs::read_to_string(&state_file).is_ok_and(|s| s == fp) {
    ui.println(Some(&StepStatus::Info), "Nothing to do")?;
    return Ok(());
  }

  let want_static = matches!(project.ty, proj_parse::ProjectType::BinaryStatic);
  let dep_libs = deps::build_deps(
    &ui,
    ctx.project,
    &ctx.dir,
    &ctx.root,
    ctx.profile,
    want_static,
  )?;
  let (pkg_cflags, pkg_libs) =
    deps::resolve_pkg_config(ctx.project, &ctx.dir, &ctx.root, want_static)?;

  let (objects, entries): (Vec<PathBuf>, Vec<CompileEntry>) =
    compile::compile_entries(&ctx.dir, &ctx.root, ctx.project, ctx.profile, pkg_cflags)?
      .into_iter()
      .unzip();

  let compile = project
    .compile
    .as_ref()
    .ok_or_else(|| miette::miette!("No compile section in conjure.kdl"))?;

  let ld_flags = compile::merged_flags(
    compile.ld_flags.as_ref(),
    profile.map(|(_, p)| p).and_then(|p| p.ld_flags.as_ref()),
  );

  let threads = compile
    .threads
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));

  compile::compile(&ui, &ctx.dir, entries, threads)?;

  let inputs = link::LinkInputs {
    objects: &objects,
    libs: &dep_libs,
    extra_libs: &pkg_libs,
    ld_flags: &ld_flags,
    compile,
  };

  link::produce(&ui, project, &ctx.dir, &ctx.root, profile_name, &inputs)?;

  let _ = fs::create_dir_all(state_file.parent().unwrap());
  let _ = fs::write(state_file, fp);
  Ok(())
}
