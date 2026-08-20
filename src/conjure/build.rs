use super::{
  compile::{self, CompileEntry},
  deps, fingerprint, link, lock,
  proj_parse::{Profile, Project},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{fs, path::PathBuf};

pub const STATE_FILE: &str = ".conjure/build/state.kdl";

pub fn build(project: &Project, profile: Option<(&str, &Profile)>) -> Result<()> {
  let root = std::env::current_dir().into_diagnostic()?;
  let ui = Ui::new();
  let profile_name = profile.map_or("default", |(name, _)| name);

  let lock = lock::LockFile::load("conjure.lock")?;
  let dep_commits: Vec<String> = lock.entries().values().map(|e| e.commit.clone()).collect();

  let fp = fingerprint::fingerprint(
    project,
    profile_name,
    &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
  )?;

  if fs::read_to_string(STATE_FILE).is_ok_and(|s| s == fp) {
    ui.println(Some(&StepStatus::Info), "Nothing to do")?;
    return Ok(());
  }

  let dep_libs = deps::build_deps(&ui, project)?;
  let (pkg_cflags, pkg_libs) = deps::resolve_pkg_config(project)?;

  let (objects, entries): (Vec<PathBuf>, Vec<CompileEntry>) =
    compile::compile_entries(&root, project, profile, pkg_cflags)?
      .into_iter()
      .unzip();

  let compile = project.compile.as_ref().ok_or_else(|| {
    miette::miette!(
      help = "Double check if there's a compile section in the conjure.kdl",
      "No compile section in conjure.kdl"
    )
  })?;

  let ld_flags = compile::merged_flags(
    compile.ld_flags.as_ref(),
    profile.map(|(_, p)| p).and_then(|p| p.ld_flags.as_ref()),
  );

  let threads = compile
    .threads
    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));

  compile::compile(&ui, entries, threads)?;

  let inputs = link::LinkInputs {
    objects: &objects,
    libs: &dep_libs,
    extra_libs: &pkg_libs,
    ld_flags: &ld_flags,
    compile,
  };

  link::produce(&ui, project, profile_name, &inputs)?;
  let _ = std::fs::create_dir_all(".conjure/build");
  let _ = std::fs::write(STATE_FILE, fp);
  Ok(())
}
