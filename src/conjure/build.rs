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
  deps, fingerprint, incremental, link, lock, proj_parse,
  proj_parse::{
    BuildSystem, Dependency, Flags, Linkage, Output, Profile, Project,
    ProjectType, Test, merge_flags,
  },
  toolchain,
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  collections::{BTreeSet, HashSet},
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
  pub record_state: bool,
  pub force: bool,
  pub threads: Option<usize>,
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

/// Which objects to recompile and whether to relink, from the previous
/// fingerprint.
struct Plan {
  compile: Vec<usize>,
  link: bool,
}

/// Without compiler depfiles a header's dependents are unknown, so any changed
/// file that is not a compiled source (an include) - and any change to the
/// non-file inputs (flags, profile, deps) - invalidates every object.
fn plan_compile(
  previous: &str,
  current: &str,
  entries: &[(PathBuf, CompileEntry)],
  graph: &incremental::DepGraph,
  force: bool,
  artifact: &Path,
) -> Plan {
  let all: Vec<usize> = (0..entries.len()).collect();
  if force || previous.is_empty() {
    return Plan {
      compile: all,
      link: true,
    };
  }

  let (old_base, old_files) = fingerprint::split(previous);
  let (new_base, new_files) = fingerprint::split(current);
  let base_changed = old_base != new_base;

  let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
  for (path, hash) in &new_files {
    if old_files.get(path) != Some(hash) {
      changed.insert(path.clone());
    }
  }
  for path in old_files.keys() {
    if !new_files.contains_key(path) {
      changed.insert(path.clone());
    }
  }

  let compile = if base_changed {
    all
  } else {
    entries
      .iter()
      .enumerate()
      .filter_map(|(i, (obj, entry))| {
        // No dep record yet (a new target, or a pre-depfile object) => rebuild.
        let dep_changed = graph
          .objects
          .get(obj)
          .is_none_or(|deps| deps.iter().any(|d| changed.contains(d)));
        (!obj.exists()
          || changed.contains(&PathBuf::from(entry.source()))
          || dep_changed)
          .then_some(i)
      })
      .collect()
  };

  let prev_objects: BTreeSet<&PathBuf> = graph.objects.keys().collect();
  let curr_objects: BTreeSet<&PathBuf> =
    entries.iter().map(|(obj, _)| obj).collect();

  let link = !compile.is_empty()
    || base_changed
    || prev_objects != curr_objects
    || !artifact.exists();

  Plan { compile, link }
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
  let mut project = ctx.project.with_profile(ctx.resolve().map(|(_, p)| p));
  if let Some(threads) = ctx.threads
    && let Some(compile) = project.compile.as_mut()
  {
    compile.threads = Some(threads);
  }

  let lock = lock::LockFile::load(ctx.dir.join("conjure.lock"))?;
  let dep_commits: Vec<String> =
    lock.entries().values().map(|e| e.commit.clone()).collect();
  let mut file_cache = fingerprint::FileCache::load(&ctx.dir);

  // pkg-config cflags are compile inputs, so they belong in the fingerprint.
  // Their search dirs live under the deps, which a cold tree hasn't built yet:
  // resolve early for the fast path, and again for real after `build_deps`.

  let early_pkg = deps::resolve_pkg_config(&project, &ctx.dir, &ctx.root).ok();
  let early_cflags = early_pkg
    .as_ref()
    .map(|(cflags, _)| cflags.as_slice())
    .unwrap_or(&[]);

  let mut fp = fingerprint::fingerprint(
    &project,
    &ctx.dir,
    profile_name,
    &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
    early_cflags,
    &mut file_cache,
  )?;

  file_cache.save(&ctx.dir);

  let state_file = ctx.dir.join(".conjure/build/state.kdl");
  let previous = fs::read_to_string(&state_file).unwrap_or_default();
  let artifact = built_artifact(&project, &ctx.dir, &ctx.root, profile_name);
  let up_to_date = ctx.record_state
    && early_pkg.is_some()
    && !ctx.force
    && previous == fp
    && artifact.exists();

  if up_to_date {
    ui.println(Some(&StepStatus::Info), "Nothing to do")?;
    return Ok(());
  }

  let dep_libs =
    deps::build_deps(ui, &project, &ctx.dir, &ctx.root, ctx.profile)?;

  let (pkg_cflags, pkg_libs) = match early_pkg {
    Some(resolved) => resolved,
    None => {
      let (cflags, libs) =
        deps::resolve_pkg_config(&project, &ctx.dir, &ctx.root)?;
      fp = fingerprint::fingerprint(
        &project,
        &ctx.dir,
        profile_name,
        &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
        &cflags,
        &mut file_cache,
      )?;
      (cflags, libs)
    }
  };

  let entries = compile::compile_entries(
    &ctx.dir,
    &ctx.root,
    &project,
    profile_name,
    pkg_cflags.clone(),
    true,
  )?;

  let mut deps_graph = incremental::DepGraph::load(&ctx.dir, profile_name);

  let plan =
    plan_compile(&previous, &fp, &entries, &deps_graph, ctx.force, &artifact);
  let objects: Vec<PathBuf> =
    entries.iter().map(|(obj, _)| obj.clone()).collect();

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

  let tc = toolchain::resolve(compile, &project.language);

  // Only the objects compiled this run wrote a depfile.
  let compiled: Vec<PathBuf> =
    plan.compile.iter().map(|&i| entries[i].0.clone()).collect();

  if !plan.compile.is_empty() {
    let selected: Vec<CompileEntry> = entries
      .into_iter()
      .enumerate()
      .filter_map(|(i, (_, entry))| plan.compile.contains(&i).then_some(entry))
      .collect();
    compile::compile(ui, &ctx.dir, selected, threads, tc.env())?;
  }

  if ctx.record_state && !compiled.is_empty() {
    for obj in &compiled {
      let (depfile, _) = tc.depfile(&obj.display().to_string());
      if let Ok(text) = fs::read_to_string(&depfile) {
        deps_graph
          .objects
          .insert(obj.clone(), tc.parse_depfile(&text));
        let _ = fs::remove_file(&depfile);
      }
    }
    let current: HashSet<&PathBuf> = objects.iter().collect();
    deps_graph.objects.retain(|obj, _| current.contains(obj));
    deps_graph.save(&ctx.dir, profile_name);

    // The graph now names this build's headers, and `fingerprint` hashes them,
    // so re-key with it; otherwise the next run sees a dep-file delta and
    // rebuilds once more.
    fp = fingerprint::fingerprint(
      &project,
      &ctx.dir,
      profile_name,
      &dep_commits.iter().map(String::as_str).collect::<Vec<_>>(),
      &pkg_cflags,
      &mut file_cache,
    )?;
    file_cache.save(&ctx.dir);
  }

  if plan.link {
    let inputs = link::LinkInputs {
      objects: &objects,
      libs: &dep_libs,
      extra_libs: &pkg_libs,
      ld_flags: &ld_flags,
      compile,
    };

    link::produce(ui, &project, &ctx.dir, &ctx.root, profile_name, &inputs)?;
  }

  if ctx.record_state {
    let _ = fs::create_dir_all(state_file.parent().unwrap());
    let _ = fs::write(state_file, fp);
  }

  Ok(())
}

fn test_profile(p: Profile) -> Profile {
  Profile {
    ty: None,
    link: None,
    src: None,
    tests: None,
    artifact: None,
    generate_pc: None,
    ..p.clone()
  }
}

pub fn test_project(
  parent: &Project,
  name: &str,
  test: &Test,
  profile: Option<&str>,
) -> Result<Project> {
  let src = test.src.clone().ok_or_else(|| {
    miette::miette!(
      help = "Test `{name}` must have a `src` specified",
      "Test `{name}` has no `src`"
    )
  })?;

  let mut compile = parent.compile.clone().unwrap_or_default();
  compile.src = Some(src);
  compile.include =
    merge_flags(compile.include.as_ref(), test.include.as_ref());
  compile.c_flags =
    merge_flags(compile.c_flags.as_ref(), test.c_flags.as_ref());
  compile.ld_flags =
    merge_flags(compile.ld_flags.as_ref(), test.ld_flags.as_ref());

  let mut dependencies = parent.dependencies.clone().unwrap_or_default();
  dependencies.insert(
    parent.name.clone(),
    Dependency {
      local: Some(".".into()),
      build: Some(BuildSystem::Conjure(None)),
      ..Default::default()
    },
  );

  let profiles = parent.profiles.as_ref().map(|profiles| {
    let mut profiles = profiles.clone();
    if let Some(active) = profile
      && let Some(p) = profiles.get(active).cloned()
    {
      profiles.insert(active.to_string(), test_profile(p));
    }
    profiles
  });

  let output = parent.output.as_ref();
  let bin = output
    .and_then(|o| o.bin.clone())
    .unwrap_or_else(|| "bin".into());

  let output_bin = PathBuf::from(bin).join(name).display().to_string();

  Ok(Project {
    name: name.to_string(),
    language: parent.language.clone(),
    ty: ProjectType::Binary,
    link: Linkage::Dynamic,
    compile: Some(compile),
    profiles,
    dependencies: Some(dependencies),
    output: Some(Output {
      bin: Some(output_bin),
      lib: None,
      symlink_binaries: output.and_then(|o| o.symlink_binaries),
    }),
    ..Default::default()
  })
}

/// Build a project and all of its siblings under the current directory.
pub fn build(
  project: &Project,
  profile: Option<&str>,
  force: bool,
  siblings: bool,
  threads: Option<usize>,
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
      threads,
      record_state: true,
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
          threads,
          record_state: true,
        },
        &ui,
      )?;
    }
  }
  Ok(())
}

pub fn test(
  project: &Project,
  profile: Option<&str>,
  names: &[String],
  threads: Option<usize>,
) -> Result<()> {
  let resolved = profile.and_then(|name| project.profile(name));
  let effective = project.with_profile(resolved.map(|(_, p)| p));

  let tests = effective
    .tests
    .as_ref()
    .filter(|t| !t.is_empty())
    .ok_or_else(|| miette::miette!("No `tests` in conjure.kdl"))?;

  miette::ensure!(
    effective.is_library(),
    help = "Tests link the project's library, so the selected profile must be `type library`",
    "`{}` is not a library under this profile",
    project.name
  );
  for want in names {
    miette::ensure!(tests.contains_key(want), "test `{want}` not found");
  }

  let cwd = std::env::current_dir().into_diagnostic()?;
  let ui = Ui::new();

  let mut selected: Vec<(&String, &Test)> = tests
    .iter()
    .filter(|(name, _)| names.is_empty() || names.iter().any(|n| n == *name))
    .collect();
  selected.sort_by(|a, b| a.0.cmp(b.0));

  for (name, target) in selected {
    // `test_project` stays on the *raw* parent so `build_ctx` applies the
    // profile once; it only uses `effective` for the tests lookup and the
    // library check.
    let child = test_project(project, name, target, profile)?;
    build_ctx(
      &BuildCtx {
        project: &child,
        dir: cwd.clone(),
        root: cwd.clone(),
        profile,
        force: false,
        threads,
        record_state: false,
      },
      &ui,
    )?;
  }
  Ok(())
}
