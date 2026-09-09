//! Linking and artifact placement.
//!
//! Decides between the compiler driver and a directly driven raw linker, builds
//! the link argv via the resolved [`Toolchain`], then writes the artifact to its
//! final `bin`/`lib` location. Static libraries are archived instead of linked.
//!
//! The raw-link path is preferred when the toolchain supports it and the flags
//! are linker-native; driver-only flags (`-flto`, `-Wl,...`) fall back to the
//! compiler driver. A raw attempt that fails or cannot spawn degrades to the
//! driver as well, so a raw linker need only be present, not perfect.
//!
//! Artifact layout is `bin|lib/<project>/<profile>/<name>` when the project
//! shares its tree with others (siblings, conjure deps) and `bin|lib/<profile>/`
//! when it is the lone project. See [`output_scoped`].
//!
//! Dependency libraries are linked on both the driver and raw paths, so a
//! shared library's `NEEDED` entries do not depend on which linker ran.

/***********************************************************************/

use super::{
  proj_parse::{Compile, Project},
  toolchain::{self, Toolchain},
  ui::{StepStatus, Ui},
};
use miette::{IntoDiagnostic, Result};
use std::{
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

/// Whether a library is shared or a binary is statically linked; drives the
/// archive variant of the li
#[derive(Clone, Copy)]
enum LinkKind {
  Binary { is_static: bool },
  Shared,
}

/// How a link will be performed: through the compiler driver, or by driving the
/// linker directly with a probed closure.
enum LinkPlan {
  Driver { prefix: Vec<String> },
  Raw { program: String },
}

/// Everything a single link invocation needs, beyond its inputs.
struct LinkSpec<'a> {
  ui: &'a Ui,
  tc: &'a Toolchain,
  dir: &'a Path,
  out: &'a str,
  what: String,
  kind: LinkKind,
}

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

fn binary_file_name(base: &str) -> String {
  if cfg!(windows) {
    format!("{base}.exe")
  } else {
    base.to_string()
  }
}

fn shared_file_name(base: &str) -> String {
  if cfg!(windows) {
    format!("{base}.dll")
  } else if cfg!(target_os = "macos") {
    format!("lib{base}.dylib")
  } else {
    format!("lib{base}.so")
  }
}

/// The toolchain for a project, resolved without probing: only used to pick
/// artifact names (static archive spelling), which needs no environment.
fn project_toolchain(project: &Project) -> Toolchain {
  let compile = project.compile.as_ref().cloned().unwrap_or_default();
  toolchain::resolve(&compile, &project.language)
}

fn static_token(tc: &Toolchain, is_static: bool) -> Option<String> {
  if is_static { tc.static_flag() } else { None }
}

fn split_ws(s: &str) -> Vec<String> {
  s.split_whitespace().map(String::from).collect()
}

fn is_static_flag(f: &str) -> bool {
  matches!(f, "-static" | "-Wl,-static")
}

fn driver_argv(
  kind: LinkKind,
  tc: &Toolchain,
  inputs: &LinkInputs<'_>,
  flags: &[String],
  prefix: &[String],
  out: &str,
) -> Vec<String> {
  let job = toolchain::LinkJob {
    shared: matches!(kind, LinkKind::Shared),
    static_flag: match kind {
      LinkKind::Binary { is_static: true } => static_token(tc, true),
      _ => None,
    },
    prefix,
    objects: inputs.objects,
    libs: inputs.libs,
    extra_libs: inputs.extra_libs,
    flags,
    out,
  };
  tc.driver_link_args(&job)
}

fn link_plan(
  tc: &Toolchain,
  configured: Option<&str>,
  driver_only: bool,
) -> Result<LinkPlan> {
  match configured {
    // A whitespace-bearing `linker` is a legacy driver-prefix command, e.g.
    // "gcc -fuse-ld=mold".
    Some(l) if l.chars().any(char::is_whitespace) => Ok(LinkPlan::Driver {
      prefix: split_ws(l),
    }),
    Some(l) => Ok(LinkPlan::Raw {
      program: l.to_string(),
    }),
    None if driver_only => Ok(LinkPlan::Driver {
      prefix: tc.cc.clone(),
    }),
    None => {
      let program = tc.default_linker()?;
      // Cygwin gcc prints a POSIX linker path a native PE can't spawn; the
      // driver is the only workable path there. Native MinGW-w64 prints a
      // spawnable C:/ path and keeps the raw path.
      if cfg!(windows) && program.starts_with('/') {
        return Ok(LinkPlan::Driver {
          prefix: tc.cc.clone(),
        });
      }
      Ok(LinkPlan::Raw { program })
    }
  }
}

fn try_raw_link(
  ui: &Ui,
  dir: &Path,
  argv: &[String],
  env: &[(OsString, OsString)],
) -> Result<bool> {
  Ok(ui.try_exec_env(dir, argv, env)?.is_none())
}

#[cfg(unix)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::unix::fs::symlink(target, link).into_diagnostic()
}

#[cfg(windows)]
fn symlink_bin(target: &Path, link: &Path) -> Result<()> {
  std::os::windows::fs::symlink_file(target, link).into_diagnostic()
}

fn run_driver(
  spec: &LinkSpec<'_>,
  inputs: &LinkInputs<'_>,
  flags: &[String],
  prefix: &[String],
) -> Result<()> {
  let argv = driver_argv(spec.kind, spec.tc, inputs, flags, prefix, spec.out);
  spec.ui.run_step_env(
    format!("Linking {}", spec.what),
    spec.dir,
    &argv,
    spec.tc.env(),
  )
}

fn run_link(
  spec: &LinkSpec<'_>,
  inputs: &LinkInputs<'_>,
  flags: &[String],
  configured: Option<&str>,
) -> Result<()> {
  let driver_only = configured.is_none()
    && (!spec.tc.supports_raw_link()
      || flags.iter().any(|f| spec.tc.needs_driver(f)));

  match link_plan(spec.tc, configured, driver_only)? {
    LinkPlan::Driver { prefix } => run_driver(spec, inputs, flags, &prefix)?,

    LinkPlan::Raw { program } => {
      let explicit = configured.is_some();
      let job = toolchain::LinkJob {
        shared: matches!(spec.kind, LinkKind::Shared),
        static_flag: match spec.kind {
          LinkKind::Binary { is_static: true } => spec.tc.static_flag(),
          _ => None,
        },
        prefix: std::slice::from_ref(&program),
        objects: inputs.objects,
        libs: inputs.libs,
        extra_libs: inputs.extra_libs,
        flags,
        out: spec.out,
      };
      match spec.tc.raw_link_args(&job) {
        Ok(argv) => {
          match try_raw_link(spec.ui, spec.dir, &argv, spec.tc.env()) {
            Ok(true) => {}
            Ok(false) => {
              spec.ui.println(
                Some(&StepStatus::Info),
                format!(
                  "Raw link with {program} failed; using the compiler driver"
                ),
              )?;
              run_driver(spec, inputs, flags, &spec.tc.cc)?;
            }
            // Only an explicitly configured linker surfaces its own failure;
            // an unconfigured one degrades to the driver.
            Err(e) if explicit => return Err(e),
            Err(_) => {
              spec.ui.println(
              Some(&StepStatus::Info),
              format!("Raw linker {program} is unavailable; using the compiler driver"),
            )?;
              run_driver(spec, inputs, flags, &spec.tc.cc)?;
            }
          }
        }
        Err(e) if explicit => return Err(e),
        Err(_) => {
          spec.ui.println(
            Some(&StepStatus::Info),
            format!("Linker closure probe failed for {program}; using the compiler driver"),
          )?;
          run_driver(spec, inputs, flags, &spec.tc.cc)?;
        }
      }
    }
  }

  Ok(())
}

fn link_binary(
  ui: &Ui,
  tc: &Toolchain,
  project: &Project,
  target: &LinkTarget,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let is_static = project.want_static();

  // A dynamic binary must not carry whole-program static flags; strip any that
  // leaked in from the base or a profile.
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
          "Ignored static link flag{} for dynamic binary `{}`: {}",
          if removed.len() > 1 { "s" } else { "" },
          project.name,
          removed.join(" ")
        ),
      )?;
    }
  }

  let out = target
    .out
    .strip_prefix(target.dir)
    .unwrap_or(target.out)
    .to_str()
    .unwrap();
  let spec = LinkSpec {
    ui,
    tc,
    dir: target.dir,
    out,
    what: target.out.display().to_string(),
    kind: LinkKind::Binary { is_static },
  };
  let configured = inputs
    .compile
    .linker
    .as_deref()
    .filter(|s| !s.trim().is_empty());

  run_link(&spec, inputs, &flags, configured)?;

  if target.symlink {
    symlink_bin(
      target.out,
      &target.root.join(binary_file_name(&project.name)),
    )?;
  }

  Ok(())
}

fn link_shared(
  ui: &Ui,
  tc: &Toolchain,
  target: &LinkTarget,
  inputs: &LinkInputs<'_>,
) -> Result<()> {
  let out = target
    .out
    .strip_prefix(target.dir)
    .unwrap_or(target.out)
    .to_str()
    .unwrap();
  let spec = LinkSpec {
    ui,
    tc,
    dir: target.dir,
    out,
    what: target.out.display().to_string(),
    kind: LinkKind::Shared,
  };
  let configured = inputs
    .compile
    .linker
    .as_deref()
    .filter(|s| !s.trim().is_empty());

  run_link(&spec, inputs, inputs.ld_flags, configured)
}

/// Whether artifact paths include the project-name level. True when other
/// projects share the same output tree (co-built siblings, or any non-root dir).
pub fn output_scoped(project: &Project, dir: &Path, root: &Path) -> bool {
  dir != root || project.siblings.as_ref().is_some_and(|s| !s.is_empty())
}

/// The exact binary path [`produce`] writes, honoring `output.bin`.
pub fn binary_path(
  project: &Project,
  root: &Path,
  profile_name: &str,
  scoped: bool,
) -> PathBuf {
  let output = project.output.as_ref();
  let mut out_dir =
    root.join(output.and_then(|o| o.bin.as_deref()).unwrap_or("bin"));
  if scoped {
    out_dir = out_dir.join(&project.name);
  }
  out_dir
    .join(profile_name)
    .join(binary_file_name(&project.name))
}

/// The exact library path [`produce`] writes, honoring `output.lib`. The archive
/// name depends on the toolchain (`.a` vs `.lib`), so this resolves the driver.
pub fn library_path(
  project: &Project,
  root: &Path,
  profile_name: &str,
  scoped: bool,
) -> PathBuf {
  let output = project.output.as_ref();
  let mut out_dir =
    root.join(output.and_then(|o| o.lib.as_deref()).unwrap_or("lib"));

  if scoped {
    out_dir = out_dir.join(&project.name);
  }

  let out_dir = out_dir.join(profile_name);
  if project.is_library() {
    if project.want_static() {
      out_dir
        .join(project_toolchain(project).static_archive_name(&project.name))
    } else {
      out_dir.join(shared_file_name(&project.name))
    }
  } else {
    unreachable!("library_path called on a non-library project")
  }
}

/// Produce the project's artifact: archive a static library, link a shared
/// library, or link a binary, then report its path.
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
  let tc = toolchain::resolve(inputs.compile, &project.language);
  let scoped = output_scoped(project, dir, root);

  if project.is_library() {
    if project.want_static() {
      let lib = library_path(project, root, profile_name, scoped);
      fs::create_dir_all(lib.parent().unwrap()).into_diagnostic()?;
      let rel_lib = lib.strip_prefix(dir).unwrap_or(&lib);
      let rel = rel_lib.to_str().unwrap();
      let objects: Vec<&str> =
        inputs.objects.iter().filter_map(|p| p.to_str()).collect();
      for (header, argv) in tc.archive(rel, &objects) {
        ui.run_step_env(
          format!("{header} {}", lib.display()),
          dir,
          &argv,
          tc.env(),
        )?;
      }
      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", lib.display()),
      )?;
    } else {
      let lib = library_path(project, root, profile_name, scoped);
      fs::create_dir_all(lib.parent().unwrap()).into_diagnostic()?;
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
      ui.println(
        Some(&StepStatus::Success),
        format!("Built {}", lib.display()),
      )?;
    }
  } else {
    let bin = binary_path(project, root, profile_name, scoped);
    fs::create_dir_all(bin.parent().unwrap()).into_diagnostic()?;
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
    ui.println(
      Some(&StepStatus::Success),
      format!("Built {}", bin.display()),
    )?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{
    super::{
      link::output_scoped,
      proj_parse::{Linkage, Project, ProjectType, Sibling},
      toolchain::{self, Gnu, Microsoft, Toolchain},
    },
    binary_file_name, library_path, shared_file_name,
  };
  use std::{
    collections::HashMap,
    path::{Path, PathBuf},
  };

  impl Toolchain {
    pub fn gnu(cc: Vec<String>) -> Self {
      Self {
        cc,
        driver: Box::new(Gnu::default()),
      }
    }
    pub fn microsoft(cc: Vec<String>) -> Self {
      Self {
        cc,
        driver: Box::new(Microsoft::default()),
      }
    }
  }

  #[test]
  fn gnu_driver_flag_classification() {
    let tc = Toolchain::gnu(vec!["gcc".into()]);

    assert!(tc.needs_driver("-flto"));
    assert!(tc.needs_driver("-static-libgcc"));
    assert!(!tc.needs_driver("-Wl,-rpath,/x"));
    assert!(!tc.needs_driver("-pthread"));
    assert!(!tc.needs_driver("-lz"));
  }

  #[test]
  fn msvc_raw_binary_uses_out() {
    let tc = Toolchain::microsoft(vec!["cl".into()]);
    let obj = PathBuf::from("a.obj");
    let job = toolchain::LinkJob {
      shared: false,
      static_flag: None,
      prefix: &["link.exe".into()],
      objects: std::slice::from_ref(&obj),
      libs: &[],
      extra_libs: &[],
      flags: &["/SUBSYSTEM:CONSOLE".into()],
      out: "app.exe",
    };

    let argv = tc.raw_link_args(&job).unwrap();
    assert_eq!(
      argv,
      vec![
        "link.exe",
        "/nologo",
        "a.obj",
        "/OUT:app.exe",
        "/SUBSYSTEM:CONSOLE"
      ]
    );
  }

  #[test]
  fn msvc_raw_shared_adds_dll() {
    let tc = Toolchain::microsoft(vec!["cl".into()]);
    let obj = PathBuf::from("a.obj");
    let job = toolchain::LinkJob {
      shared: true,
      static_flag: None,
      prefix: &["link.exe".into()],
      objects: std::slice::from_ref(&obj),
      libs: &[],
      extra_libs: &[],
      flags: &[],
      out: "mylib.dll",
    };

    let argv = tc.raw_link_args(&job).unwrap();
    assert_eq!(
      argv,
      vec!["link.exe", "/nologo", "/DLL", "a.obj", "/OUT:mylib.dll"]
    );
  }

  #[test]
  fn output_scoped_requires_siblings_or_shared_root() {
    let mut project = crate::conjure::proj_parse::Project::default();
    let root = Path::new("/r");
    assert!(!output_scoped(&project, root, root)); // lone project

    project.siblings = Some(HashMap::new());
    assert!(!output_scoped(&project, root, root)); // empty = no siblings

    project.siblings = Some(HashMap::from([(
      "sub".to_string(),
      Sibling { path: "sub".into() },
    )]));

    assert!(output_scoped(&project, root, root)); // has siblings

    project.siblings = None;
    assert!(output_scoped(&project, Path::new("/r/sub"), root)); // co-built
  }

  fn gnu_tc() -> Toolchain {
    Toolchain::gnu(vec!["gcc".into()])
  }

  #[test]
  fn gnu_link_args_static_binary_shape() {
    let obj = PathBuf::from("a.o");
    let tc = gnu_tc();
    let job = toolchain::LinkJob {
      shared: false,
      static_flag: tc.static_flag(),
      prefix: &["gcc".into()],
      objects: std::slice::from_ref(&obj),
      libs: &[],
      extra_libs: &[],
      flags: &[],
      out: "out",
    };

    assert_eq!(
      tc.driver_link_args(&job),
      vec!["gcc", "-static", "a.o", "-o", "out"]
    );
  }

  fn msvc_tc() -> Toolchain {
    Toolchain::microsoft(vec!["cl".into()])
  }

  #[test]
  fn msvc_link_args_uses_fe_and_link() {
    let obj = PathBuf::from("a.obj");
    let tc = msvc_tc();
    let job = toolchain::LinkJob {
      shared: false,
      static_flag: Some("/MT".into()),
      prefix: &["cl".into()],
      objects: std::slice::from_ref(&obj),
      libs: &[],
      extra_libs: &[],
      flags: &["/SUBSYSTEM:CONSOLE".into()],
      out: "app.exe",
    };

    let argv = tc.driver_link_args(&job);
    assert!(argv.contains(&"/MT".into()));
    assert!(argv.contains(&"/Fe:app.exe".into()));

    let link = argv.iter().position(|a| a == "/link").unwrap();
    assert_eq!(argv[link + 1], "/SUBSYSTEM:CONSOLE");
  }

  #[test]
  fn msvc_shared_link_args_uses_ld() {
    let obj = PathBuf::from("a.obj");
    let tc = msvc_tc();
    let job = toolchain::LinkJob {
      shared: true,
      static_flag: None,
      prefix: &["cl".into()],
      objects: std::slice::from_ref(&obj),
      libs: &[],
      extra_libs: &[],
      flags: &[],
      out: "mylib.dll",
    };

    assert_eq!(
      tc.driver_link_args(&job),
      vec!["cl", "/nologo", "/LD", "a.obj", "/Fe:mylib.dll"]
    )
  }

  #[test]
  fn artifact_names_are_platform_correct() {
    if cfg!(windows) {
      assert_eq!(binary_file_name("app"), "app.exe");
      assert_eq!(shared_file_name("x"), "x.dll");
    } else {
      assert_eq!(binary_file_name("app"), "app");
      #[cfg(target_os = "macos")]
      assert_eq!(shared_file_name("x"), "libx.dylib");
      #[cfg(not(target_os = "macos"))]
      assert_eq!(shared_file_name("x"), "libx.so");
    }
  }

  #[test]
  fn library_path_scoped_and_unscoped() {
    let project = Project {
      name: "foo".into(),
      ty: ProjectType::Library,
      link: Linkage::Dynamic,
      ..Default::default()
    };

    let root = Path::new("/root");
    let unscoped = library_path(&project, root, "default", false);
    let scoped = library_path(&project, root, "default", true);

    assert_eq!(
      unscoped,
      root
        .join("lib")
        .join("default")
        .join(shared_file_name("foo"))
    );

    assert_eq!(
      scoped,
      root
        .join("lib")
        .join("foo")
        .join("default")
        .join(shared_file_name("foo"))
    );
  }

  #[test]
  fn static_library_path_uses_archive_name() {
    let project = Project {
      name: "foo".into(),
      ty: ProjectType::Library,
      link: Linkage::Static,
      ..Default::default()
    };

    let p = library_path(&project, Path::new("/root"), "default", false);
    let file = p.file_name().unwrap().to_string_lossy().to_string();

    assert!(file.contains("foo"));
    assert!(file.ends_with(".a") || file.ends_with(".lib"));
  }
}
