//! End-to-end tests: drive the built `conjure` binary against throwaway C
//! projects. These cover the orchestration unit tests can't reach - `cli::run`
//! dispatch, `build::build_ctx`, dep resolution/building/linking, siblings,
//! profiles - by really invoking the toolchain through `conjure`.

use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, Output},
};

const CONJURE: &str = env!("CARGO_BIN_EXE_conjure");

fn tmp(name: &str) -> PathBuf {
  let base = std::env::var_os("CARGO_TARGET_TMPDIR")
    .map(PathBuf::from)
    .unwrap_or_else(std::env::temp_dir)
    .join(format!("it_{name}"));
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&base).unwrap();
  base
}

fn write(path: &Path, contents: &str) {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).unwrap();
  }
  fs::write(path, contents).unwrap();
}

fn run(dir: &Path, args: &[&str]) -> Output {
  Command::new(CONJURE)
    .current_dir(dir)
    .args(args)
    .output()
    .unwrap()
}

fn conjure(dir: &Path, args: &[&str]) -> Output {
  let out = run(dir, args);
  assert!(
    out.status.success(),
    "conjure {args:?} failed in {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
    dir.display(),
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );
  out
}

fn combined(out: &Output) -> String {
  format!(
    "{}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  )
}

fn exe(name: &str) -> String {
  if cfg!(windows) {
    format!("{name}.exe")
  } else {
    name.to_string()
  }
}

fn shared(name: &str) -> String {
  if cfg!(windows) {
    format!("{name}.dll")
  } else if cfg!(target_os = "macos") {
    format!("lib{name}.dylib")
  } else {
    format!("lib{name}.so")
  }
}

/// These build through the real `cc`/`ar`; skip quietly where there is none.
fn have_cc() -> bool {
  cfg!(windows) || Command::new("cc").arg("--version").output().is_ok()
}

fn have_git() -> bool {
  Command::new("git").arg("--version").output().is_ok()
}

fn have_make() -> bool {
  Command::new("make").arg("--version").output().is_ok()
}

fn pkg_config_has(pkg: &str) -> bool {
  Command::new("pkg-config")
    .args(["--exists", pkg])
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

/// A git repo holding a conjure library; returns the repo path for a
/// `remote git "<path>"` dependency.
fn git_upstream(base: &Path) -> PathBuf {
  let up = base.join("upstream");
  write(&up.join("conjure.kdl"), LIB_MANIFEST);
  write(
    &up.join("include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(&up.join("src/greet.c"), "int greet(void) { return 42; }\n");
  git(&up, &["init", "-q"]);
  git(&up, &["add", "."]);
  git(&up, &["commit", "-qm", "init"]);
  up
}

/// A binary consumer with a `remote git` dep on `up`.
fn git_consumer(base: &Path, up: &Path) -> PathBuf {
  let app = base.join("app");
  write(
    &app.join("conjure.kdl"),
    &format!(
      r#"project {{
  name app
  language c
  type binary
  link dynamic
  compile {{ standard c11 }}
  dependencies {{
    greet {{ remote git "{}" }}
  }}
}}
"#,
      up.display().to_string().replace('\\', "/")
    ),
  );
  write(
    &app.join("src/main.c"),
    "#include \"greet.h\"\nint main(void) { return greet() == 42 ? 0 : 1; }\n",
  );
  app
}

fn git(dir: &Path, args: &[&str]) {
  let out = Command::new("git")
    .current_dir(dir)
    .env("GIT_AUTHOR_NAME", "t")
    .env("GIT_AUTHOR_EMAIL", "t@t")
    .env("GIT_COMMITTER_NAME", "t")
    .env("GIT_COMMITTER_EMAIL", "t@t")
    .args(args)
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "git {args:?} failed: {}",
    String::from_utf8_lossy(&out.stderr)
  );
}

const LIB_MANIFEST: &str = r#"project {
  name greet
  language c
  type library
  link static
  compile {
    standard c11
    include "include"
  }
}
"#;

const BIN_MANIFEST: &str = r#"project {
  name hello
  language c
  type binary
  link dynamic
  compile {
    standard c11
  }
}
"#;

const MAIN_C: &str = r#"#include <stdio.h>
int main(void) {
  puts("hi");
  return 0;
}
"#;

#[test]
fn new_scaffold_builds() {
  if !have_cc() {
    return;
  }
  let dir = tmp("new");
  conjure(&dir, &["new", "app", "-l", "c"]);
  let app = dir.join("app");
  assert!(app.join("conjure.kdl").is_file());
  conjure(&app, &["build"]);
  assert!(app.join("bin/default").join(exe("app")).is_file());
}

#[test]
fn library_dynamic_builds() {
  if !have_cc() {
    return;
  }
  let dir = tmp("lib_dyn");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name greet
  language c
  type library
  link dynamic
  compile { standard c11 }
}
"#,
  );
  write(&dir.join("src/greet.c"), "int greet(void) { return 42; }\n");

  conjure(&dir, &["build"]);
  assert!(dir.join("lib/default").join(shared("greet")).is_file());
}

#[test]
fn binary_builds_and_reruns_are_noops() {
  if !have_cc() {
    return;
  }
  let dir = tmp("bin");
  write(&dir.join("conjure.kdl"), BIN_MANIFEST);
  write(&dir.join("src/main.c"), MAIN_C);

  conjure(&dir, &["build"]);
  assert!(dir.join("bin/default").join(exe("hello")).is_file());

  let again = conjure(&dir, &["build"]);
  assert!(
    combined(&again).contains("Nothing to do"),
    "{}",
    combined(&again)
  );

  let forced = conjure(&dir, &["build", "--force"]);
  assert!(!combined(&forced).contains("Nothing to do"));
}

#[test]
fn binary_links_static_local_dep_and_runs() {
  if !have_cc() {
    return;
  }
  let base = tmp("localdep");

  write(
    &base.join("greet/conjure.kdl"),
    r#"project {
  name greet
  language c
  type library
  link static
  compile {
    standard c11
    include "include"
  }
}
"#,
  );
  write(
    &base.join("greet/include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet/src/greet.c"),
    "int greet(void) { return 42; }\n",
  );

  write(
    &base.join("app/conjure.kdl"),
    r#"project {
  name app
  language c
  type binary
  link dynamic
  compile { standard c11 }
  dependencies {
    greet { local "../greet" }
  }
}
"#,
  );
  write(
    &base.join("app/src/main.c"),
    "#include \"greet.h\"\nint main(void) { return greet() == 42 ? 0 : 1; }\n",
  );

  let app = base.join("app");
  conjure(&app, &["build"]);
  let bin = app.join("bin/default").join(exe("app"));
  assert!(bin.is_file());

  let status = Command::new(&bin).status().unwrap();
  assert!(status.success(), "app exited with {status}");
}

#[test]
fn siblings_build_together() {
  if !have_cc() {
    return;
  }
  let dir = tmp("siblings");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name root
  language c
  type binary
  link dynamic
  compile { standard c11 }
  siblings {
    sub "sub"
  }
}
"#,
  );
  write(&dir.join("src/main.c"), MAIN_C);
  write(
    &dir.join("sub/conjure.kdl"),
    r#"project {
  name sub
  language c
  type binary
  link dynamic
  compile { standard c11 }
}
"#,
  );
  write(&dir.join("sub/src/main.c"), MAIN_C);

  conjure(&dir, &["build"]);
  // With siblings present, artifacts are scoped by project name.
  assert!(dir.join("bin/root/default").join(exe("root")).is_file());
  assert!(dir.join("bin/sub/default").join(exe("sub")).is_file());
}

#[test]
fn profile_places_artifact_under_the_profile() {
  if !have_cc() {
    return;
  }
  let dir = tmp("profile");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name hello
  language c
  type binary
  link dynamic
  compile { standard c11 }
  profiles {
    debug { c_flags -g }
  }
}
"#,
  );
  write(&dir.join("src/main.c"), MAIN_C);

  conjure(&dir, &["as", "debug", "--", "build"]);
  assert!(dir.join("bin/debug").join(exe("hello")).is_file());
}

#[test]
fn compile_commands_lists_sources() {
  if !have_cc() {
    return;
  }
  let dir = tmp("ccjson");
  write(&dir.join("conjure.kdl"), BIN_MANIFEST);
  write(&dir.join("src/main.c"), MAIN_C);

  conjure(&dir, &["compile-commands"]);
  let json = fs::read_to_string(dir.join("compile_commands.json")).unwrap();
  assert!(json.contains("main.c"), "compile_commands.json: {json}");
}

#[test]
fn missing_compile_section_is_an_error() {
  let dir = tmp("nocompile");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name bad
  language c
  type binary
  link dynamic
}
"#,
  );
  write(&dir.join("src/main.c"), MAIN_C);

  let out = run(&dir, &["build"]);
  assert!(!out.status.success());
  assert!(!combined(&out).trim().is_empty());
}

#[test]
fn init_scaffolds_in_place() {
  if !have_cc() {
    return;
  }
  let dir = tmp("init");
  conjure(&dir, &["init", "-n", "initproj", "-l", "c"]);
  assert!(dir.join("conjure.kdl").is_file());
  assert!(dir.join("src/main.c").is_file());

  conjure(&dir, &["build"]);
  assert!(dir.join("bin/default").join(exe("initproj")).is_file());
}

#[test]
fn add_then_rm_rewrites_manifest() {
  if !have_cc() {
    return;
  }
  let base = tmp("addrm");
  write(&base.join("greet/conjure.kdl"), LIB_MANIFEST);
  write(
    &base.join("greet/include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet/src/greet.c"),
    "int greet(void) { return 42; }\n",
  );

  write(&base.join("app/conjure.kdl"), BIN_MANIFEST);
  write(&base.join("app/src/main.c"), MAIN_C);
  let app = base.join("app");

  conjure(&app, &["add", "local", "../greet"]);
  let manifest = fs::read_to_string(app.join("conjure.kdl")).unwrap();
  assert!(manifest.contains("greet"), "{manifest}");

  conjure(&app, &["rm", "greet"]);
  let manifest = fs::read_to_string(app.join("conjure.kdl")).unwrap();
  assert!(!manifest.contains("greet"), "{manifest}");
}

#[test]
fn rm_missing_dependency_errors() {
  let dir = tmp("rm_missing");
  write(&dir.join("conjure.kdl"), BIN_MANIFEST);
  write(&dir.join("src/main.c"), MAIN_C);

  let out = run(&dir, &["rm", "nope"]);
  assert!(!out.status.success());
}

#[test]
fn static_library_root_builds_archive() {
  if !have_cc() {
    return;
  }
  let dir = tmp("lib_static");
  write(&dir.join("conjure.kdl"), LIB_MANIFEST);
  write(
    &dir.join("include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(&dir.join("src/greet.c"), "int greet(void) { return 42; }\n");

  conjure(&dir, &["build"]);
  assert!(dir.join("lib/default/libgreet.a").is_file());
}

#[test]
fn binary_conjure_dep_is_rejected() {
  if !have_cc() {
    return;
  }
  let base = tmp("bindep");
  write(&base.join("tool/conjure.kdl"), BIN_MANIFEST);
  write(&base.join("tool/src/main.c"), MAIN_C);

  write(
    &base.join("app/conjure.kdl"),
    r#"project {
  name app
  language c
  type binary
  link dynamic
  compile { standard c11 }
  dependencies {
    hello { local "../tool" }
  }
}
"#,
  );
  write(&base.join("app/src/main.c"), MAIN_C);

  let out = run(&base.join("app"), &["build"]);
  assert!(!out.status.success());
  assert!(
    combined(&out).contains("binary conjure"),
    "{}",
    combined(&out)
  );
}

#[test]
fn git_remote_dep_clones_locks_and_builds() {
  if !have_cc() || !have_git() {
    return;
  }
  let base = tmp("gitdep");

  // upstream: a real git repo holding a conjure library
  let up = base.join("upstream");
  write(&up.join("conjure.kdl"), LIB_MANIFEST);
  write(
    &up.join("include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(&up.join("src/greet.c"), "int greet(void) { return 42; }\n");
  git(&up, &["init", "-q"]);
  git(&up, &["add", "."]);
  git(&up, &["commit", "-qm", "init"]);

  // consumer: a generic git remote pointing at the local repo path
  let app = base.join("app");
  write(
    &app.join("conjure.kdl"),
    &format!(
      r#"project {{
  name app
  language c
  type binary
  link dynamic
  compile {{ standard c11 }}
  dependencies {{
    greet {{ remote git "{}" }}
  }}
}}
"#,
      up.display()
    ),
  );
  write(
    &app.join("src/main.c"),
    "#include \"greet.h\"\nint main(void) { return greet() == 42 ? 0 : 1; }\n",
  );

  // build clones + auto-locks the unlocked dep
  conjure(&app, &["build"]);
  assert!(
    app.join("conjure.lock").is_file(),
    "auto-lock should write a lock"
  );
  let bin = app.join("bin/default").join(exe("app"));
  assert!(bin.is_file());
  assert!(Command::new(&bin).status().unwrap().success());

  // lock re-pins offline; update re-fetches
  conjure(&app, &["lock"]);
  conjure(&app, &["update"]);
}

#[test]
fn remote_dep_reuses_cache_on_rebuild() {
  if !have_cc() || !have_git() {
    return;
  }
  let base = tmp("gitcache");
  let up = git_upstream(&base);
  let app = git_consumer(&base, &up);

  conjure(&app, &["build"]);

  // --force makes the consumer rebuild; the dep key is its commit, unchanged,
  // so the dep library comes from cache instead of being rebuilt.
  let again = conjure(&app, &["build", "--force"]);
  assert!(
    combined(&again).contains("Reusing cached"),
    "{}",
    combined(&again)
  );
}

#[test]
fn dependency_pkg_config_is_resolved() {
  if !have_cc() || !pkg_config_has("zlib") {
    return;
  }
  let base = tmp("pkgconfig");

  write(&base.join("greet/conjure.kdl"), LIB_MANIFEST);
  write(
    &base.join("greet/include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet/src/greet.c"),
    "int greet(void) { return 42; }\n",
  );

  let app = base.join("app");
  write(
    &app.join("conjure.kdl"),
    r#"project {
  name app
  language c
  type binary
  link dynamic
  compile { standard c11 }
  dependencies {
    greet {
      local "../greet"
      pkg_config "zlib"
    }
  }
}
"#,
  );
  // <zlib.h> is only reachable through the pkg-config cflags, and zlibVersion
  // only links through its libs - so this fails if resolve_pkg_config is wrong.
  write(
    &app.join("src/main.c"),
    "#include \"greet.h\"\n#include <zlib.h>\n\
     int main(void) {\n\
     \x20 return (greet() == 42 && zlibVersion()[0] != '\\0') ? 0 : 1;\n\
     }\n",
  );

  conjure(&app, &["build"]);
  let bin = app.join("bin/default").join(exe("app"));
  assert!(Command::new(&bin).status().unwrap().success());
}

const MK_MAKEFILE: &str = "all: libgreet.a\n\
libgreet.a: greet.o\n\
\tar rcs $@ $^\n\
greet.o: greet.c\n\
\tcc -c $< -o $@\n";

#[test]
fn make_dep_builds_and_links() {
  if !have_cc() || !have_make() {
    return;
  }
  let base = tmp("makedep");

  let mk = base.join("mkdep");
  write(&mk.join("Makefile"), MK_MAKEFILE);
  write(&mk.join("greet.h"), "#pragma once\nint greet(void);\n");
  write(&mk.join("greet.c"), "int greet(void) { return 42; }\n");

  let app = base.join("app");
  write(
    &app.join("conjure.kdl"),
    r#"project {
  name app
  language c
  type binary
  link dynamic
  compile { standard c11 }
  dependencies {
    mk {
      local "../mkdep"
      build make
    }
  }
}
"#,
  );
  write(
    &app.join("src/main.c"),
    "#include \"greet.h\"\nint main(void) { return greet() == 42 ? 0 : 1; }\n",
  );

  conjure(&app, &["build"]);
  let bin = app.join("bin/default").join(exe("app"));
  assert!(bin.is_file());
  assert!(Command::new(&bin).status().unwrap().success());
}
