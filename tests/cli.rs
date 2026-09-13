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

/// MSVC's linker needs an import library, which conjure doesn't emit for shared
/// libraries yet, so parent libraries link statically there; gnu/unix link the
/// `.dll`/`.so` directly.
fn parent_link() -> &'static str {
  if cfg!(target_env = "msvc") {
    "static"
  } else {
    "dynamic"
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
    &up.join("include").join("greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &up.join("src").join("greet.c"),
    "int greet(void) { return 42; }\n",
  );
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
    &app.join("src").join("main.c"),
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
  assert!(app.join("bin").join("default").join(exe("app")).is_file());
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
  write(
    &dir.join("src").join("greet.c"),
    "int greet(void) { return 42; }\n",
  );

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
  write(&dir.join("src").join("main.c"), MAIN_C);

  conjure(&dir, &["build"]);
  assert!(dir.join("bin").join("default").join(exe("hello")).is_file());

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
    &base.join("greet").join("conjure.kdl"),
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
    &base.join("greet").join("include").join("greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet").join("src").join("greet.c"),
    "int greet(void) { return 42; }\n",
  );

  write(
    &base.join("app").join("conjure.kdl"),
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
    &base.join("app").join("src").join("main.c"),
    "#include \"greet.h\"\nint main(void) { return greet() == 42 ? 0 : 1; }\n",
  );

  let app = base.join("app");
  conjure(&app, &["build"]);
  let bin = app.join("bin").join("default").join(exe("app"));
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
  write(&dir.join("src").join("main.c"), MAIN_C);
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
  write(&dir.join("sub").join("src").join("main.c"), MAIN_C);

  conjure(&dir, &["build"]);
  // With siblings present, artifacts are scoped by project name.
  assert!(
    dir
      .join("bin")
      .join("root")
      .join("default")
      .join(exe("root"))
      .is_file()
  );
  assert!(
    dir
      .join("bin")
      .join("sub")
      .join("default")
      .join(exe("sub"))
      .is_file()
  );
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
  write(&dir.join("src").join("main.c"), MAIN_C);

  conjure(&dir, &["as", "debug", "--", "build"]);
  assert!(dir.join("bin").join("debug").join(exe("hello")).is_file());
}

#[test]
fn compile_commands_lists_sources() {
  if !have_cc() {
    return;
  }
  let dir = tmp("ccjson");
  write(&dir.join("conjure.kdl"), BIN_MANIFEST);
  write(&dir.join("src").join("main.c"), MAIN_C);

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
  write(&dir.join("src").join("main.c"), MAIN_C);

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
  assert!(dir.join("src").join("main.c").is_file());

  conjure(&dir, &["build"]);
  assert!(
    dir
      .join("bin")
      .join("default")
      .join(exe("initproj"))
      .is_file()
  );
}

#[test]
fn add_then_rm_rewrites_manifest() {
  if !have_cc() {
    return;
  }
  let base = tmp("addrm");
  write(&base.join("greet").join("conjure.kdl"), LIB_MANIFEST);
  write(
    &base.join("greet").join("include").join("greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet").join("src").join("greet.c"),
    "int greet(void) { return 42; }\n",
  );

  write(&base.join("app").join("conjure.kdl"), BIN_MANIFEST);
  write(&base.join("app").join("src").join("main.c"), MAIN_C);
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
  write(&dir.join("src").join("main.c"), MAIN_C);

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
    &dir.join("include").join("greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &dir.join("src").join("greet.c"),
    "int greet(void) { return 42; }\n",
  );

  conjure(&dir, &["build"]);
  let archive = if cfg!(target_env = "msvc") {
    "greet.lib"
  } else {
    "libgreet.a"
  };
  assert!(dir.join("lib").join("default").join(archive).is_file());
}

#[test]
fn binary_conjure_dep_is_rejected() {
  if !have_cc() {
    return;
  }
  let base = tmp("bindep");
  write(&base.join("tool").join("conjure.kdl"), BIN_MANIFEST);
  write(&base.join("tool").join("src").join("main.c"), MAIN_C);

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
  write(&base.join("app").join("src").join("main.c"), MAIN_C);

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
  let up = git_upstream(&base);
  let app = git_consumer(&base, &up);

  // build clones + auto-locks the unlocked dep
  conjure(&app, &["build"]);
  assert!(
    app.join("conjure.lock").is_file(),
    "auto-lock should write a lock"
  );
  let bin = app.join("bin").join("default").join(exe("app"));
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

  write(&base.join("greet").join("conjure.kdl"), LIB_MANIFEST);
  write(
    &base.join("greet/include/greet.h"),
    "#pragma once\nint greet(void);\n",
  );
  write(
    &base.join("greet").join("src").join("greet.c"),
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
    &app.join("src").join("main.c"),
    "#include \"greet.h\"\n#include <zlib.h>\n\
     int main(void) {\n\
     \x20 return (greet() == 42 && zlibVersion()[0] != '\\0') ? 0 : 1;\n\
     }\n",
  );

  conjure(&app, &["build"]);
  let bin = app.join("bin").join("default").join(exe("app"));
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
  let bin = app.join("bin").join("default").join(exe("app"));
  assert!(bin.is_file());
  assert!(Command::new(&bin).status().unwrap().success());
}

#[test]
fn sibling_profiles_resolve_independently() {
  if !have_cc() {
    return;
  }
  let dir = tmp("sib_profiles");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name root
  language c
  type binary
  link dynamic
  compile { standard c11 }
  profiles { dev { c_flags "-DROOT" } }
  siblings { sib "sib" }
}
"#,
  );
  // root: ROOT set, SIB not
  write(
    &dir.join("src").join("main.c"),
    "#ifndef ROOT\n#error Root missing ROOT\n#endif\n#ifdef SIB\n#error root leaked SIB\n#endif\nint main(void){return 0;}\n",
  );
  write(
    &dir.join("sib").join("conjure.kdl"),
    r#"project {
  name sib
  language c
  type binary
  link dynamic
  compile { standard c11 }
  profiles { dev { c_flags "-DSIB" } }
}
"#,
  );
  // sib: SIB set, ROOT not
  write(
    &dir.join("sib").join("src").join("main.c"),
    "#ifndef SIB\n#error Sib missing SIB\n#endif\n#ifdef ROOT\n#error Sib leaked ROOT\n#endif\nint main(void){return 0;}\n",
  );

  conjure(&dir, &["build", "-p", "dev"]);
  assert!(dir.join("bin/root/dev").join(exe("root")).is_file());
  assert!(dir.join("bin/sib/dev").join(exe("sib")).is_file());
}

#[test]
fn sibling_without_profile_builds_default() {
  if !have_cc() {
    return;
  }
  let dir = tmp("sib_default");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name root
  language c
  type binary
  link dynamic
  compile { standard c11 }
  profiles { dev { c_flags "-DROOT" } }
  siblings { sib "sib" }
}
"#,
  );
  write(
    &dir.join("src").join("main.c"),
    "int main(void){return 0;}\n",
  );
  write(
    &dir.join("sib/conjure.kdl"),
    r#"project {
  name sib
  language c
  type binary
  link dynamic
  compile { standard c11 }
}
"#,
  );
  // sibling has no `dev`, so it must build default and must NOT see ROOT
  write(
    &dir.join("sib").join("src").join("main.c"),
    "#ifdef ROOT\n#error Sibling inherited ROOT\n#endif\nint main(void){return 0;}\n",
  );

  let out = conjure(&dir, &["build", "-p", "dev"]);
  assert!(combined(&out).contains("no `dev` profile"));
  assert!(
    dir
      .join("bin")
      .join("root")
      .join("dev")
      .join(exe("root"))
      .is_file()
  );
  assert!(
    dir
      .join("bin")
      .join("sib")
      .join("default")
      .join(exe("sib"))
      .is_file()
  );
}

#[test]
fn no_siblings_builds_only_root() {
  if !have_cc() {
    return;
  }
  let dir = tmp("no_sibs");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name root
  language c
  type binary
  link dynamic
  compile { standard c11 }
  siblings { sib "sib" }
}
"#,
  );
  write(
    &dir.join("src").join("main.c"),
    "int main(void){return 0;}\n",
  );
  write(
    &dir.join("sib/conjure.kdl"),
    r#"project {
  name sib
  language c
  type binary
  link dynamic
  compile { standard c11 }
}
"#,
  );
  write(
    &dir.join("sib").join("src").join("main.c"),
    "int main(void){return 0;}\n",
  );

  conjure(&dir, &["build", "--no-siblings"]);
  assert!(
    dir
      .join("bin")
      .join("root")
      .join("default")
      .join(exe("root"))
      .is_file()
  );
  assert!(
    !dir
      .join("bin")
      .join("sib")
      .join("default")
      .join(exe("sib"))
      .is_file()
  );
}

#[test]
fn compile_commands_include_siblings() {
  if !have_cc() {
    return;
  }
  let dir = tmp("ccjson_sibs");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name root
  language c
  type binary
  link dynamic
  compile { standard c11 }
  siblings { sib "sib" }
}
"#,
  );
  write(&dir.join("src/main.c"), MAIN_C);
  write(
    &dir.join("sib/conjure.kdl"),
    r#"project {
  name sib
  language c
  type binary
  link dynamic
  compile { standard c11 }
}
"#,
  );
  write(&dir.join("sib/src/main.c"), MAIN_C);

  let norm = |s: &str| s.replace("\\\\", "/").replace('\\', "/");
  let sib_src = norm(
    &Path::new("sib")
      .join("src")
      .join("main.c")
      .display()
      .to_string(),
  );

  conjure(&dir, &["compile-commands"]);
  let json =
    norm(&fs::read_to_string(dir.join("compile_commands.json")).unwrap());
  assert!(json.contains("main.c"));
  assert!(json.contains(&sib_src), "Sibling sources missing: {json}");

  conjure(&dir, &["compile-commands", "--no-siblings"]);
  let json =
    norm(&fs::read_to_string(dir.join("compile_commands.json")).unwrap());
  assert!(
    !json.contains(&sib_src),
    "Unexpected sibling entries: {json}"
  );
}

#[test]
fn compile_commands_are_structured_and_cover_tests() {
  if !have_cc() {
    return;
  }
  let dir = tmp("ccjson_struct");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name core
  language c
  type library
  link dynamic
  compile { standard c11 }
  tests {
    unit { src "src/test/unit.c" }
  }
}
"#,
  );
  write(&dir.join("src/core.c"), "int core(void) { return 42; }\n");
  write(&dir.join("src/core.h"), "int core(void);\n");
  write(
    &dir.join("src/test/unit.c"),
    "#include \"../core.h\"\nint main(void) { return core() == 42 ? 0 : 1; }\n",
  );

  conjure(&dir, &["compile-commands"]);
  let text = fs::read_to_string(dir.join("compile_commands.json")).unwrap();
  let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
  assert_eq!(entries.len(), 2, "{text}");

  let canon = |p: &str| fs::canonicalize(p).unwrap();
  let root = fs::canonicalize(&dir).unwrap();
  let core = fs::canonicalize(dir.join("src").join("core.c")).unwrap();
  let unit =
    fs::canonicalize(dir.join("src").join("test").join("unit.c")).unwrap();

  for entry in &entries {
    assert_eq!(canon(entry["directory"].as_str().unwrap()), root, "{entry}");
  }

  let files: Vec<PathBuf> = entries
    .iter()
    .map(|e| canon(e["file"].as_str().unwrap()))
    .collect();
  assert_eq!(files.iter().filter(|f| **f == core).count(), 1, "{text}");
  assert_eq!(files.iter().filter(|f| **f == unit).count(), 1, "{text}");

  let compiles = |entry: &serde_json::Value, want: &Path| {
    entry["arguments"]
      .as_array()
      .unwrap()
      .iter()
      .any(|a| fs::canonicalize(a.as_str().unwrap()).is_ok_and(|p| p == want))
  };
  let entry_for = |file: &Path| {
    entries
      .iter()
      .find(|e| canon(e["file"].as_str().unwrap()) == *file)
      .unwrap()
  };

  // The project's own entry compiles core.c, not the test source.
  assert!(compiles(entry_for(&core), &core));
  assert!(!compiles(entry_for(&core), &unit));
  // The test target compiles the test source, not the project's.
  assert!(compiles(entry_for(&unit), &unit));
  assert!(!compiles(entry_for(&unit), &core));
}

/// A non-native `--arch x86` must resolve the matching `vcvarsall.bat x86`
/// environment, not the host x64 one, and actually emit a 32-bit PE.
/// Windows-msvc only; needs VS's x86 tools installed.
#[cfg(all(windows, target_env = "msvc"))]
#[test]
fn msvc_arch_uses_matching_vcvars() {
  let dir = tmp("msvc_arch");
  conjure(&dir, &["new", "app", "-l", "c", "-a", "x86"]);
  let app = dir.join("app");
  conjure(&app, &["build"]);
  let exe_path = app.join("bin").join("default").join(exe("app"));
  assert_eq!(pe_machine(&exe_path), 0x014c, "expected IR386 PE, not x64");
}

/// COFF `Machine` field (`IMAGE_FILE_MACHINE_*`) from a PE image.
#[cfg(all(windows, target_env = "msvc"))]
fn pe_machine(path: &Path) -> u16 {
  let bytes = fs::read(path).unwrap();
  let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
  assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
  u16::from_le_bytes(bytes[pe + 4..pe + 6].try_into().unwrap())
}

#[test]
fn test_targets_link_the_parent_library() {
  if !have_cc() {
    return;
  }
  let dir = tmp("tests_field");

  let manifest = r#"project {
  name core
  language c
  type library
  link PARENT_LINK
  compile { standard c11 }
  tests {
    unit { src "src/test/unit.c" }
  }
}
"#
  .replace("PARENT_LINK", parent_link());
  write(&dir.join("conjure.kdl"), &manifest);
  write(
    &dir.join("src").join("core.c"),
    "int core(void) { return 42; }\n",
  );
  write(&dir.join("src").join("core.h"), "int core(void);\n");
  write(
    &dir.join("src").join("test").join("unit.c"),
    "#include \"../core.h\"\nint main(void) { return core() == 42 ? 0 : 1; }\n",
  );

  conjure(&dir, &["test"]);
  assert!(
    dir
      .join("bin")
      .join("unit")
      .join("default")
      .join(exe("unit"))
      .is_file()
  );
}

#[test]
fn profile_tests_only_build_under_their_profile() {
  if !have_cc() {
    return;
  }
  let dir = tmp("profile_tests");
  let manifest = r#"project {
  name app
  language c
  type binary
  link dynamic
  compile { standard c11 }
  profiles {
    lib {
      type library
      link PROFILE_LINK
      tests { unit { src "src/test/unit.c" } }
    }
  }
}
"#
  .replace("PROFILE_LINK", parent_link());
  write(&dir.join("conjure.kdl"), &manifest);
  write(&dir.join("src/main.c"), "int main(void) { return 0; }\n");
  write(&dir.join("src/lib.c"), "int core(void) { return 42; }\n");
  write(&dir.join("src/core.h"), "int core(void);\n");
  write(
    &dir.join("src").join("test").join("unit.c"),
    "#include \"../core.h\"\nint main(void) { return core() == 42 ? 0 : 1; }\n",
  );

  // Base is a binary with no tests, so plain `test` has nothing to build.
  assert!(!run(&dir, &["test"]).status.success());

  // Under `lib` the project is a library and the profile's tests apply.
  conjure(&dir, &["test", "-p", "lib"]);
  assert!(
    dir
      .join("bin")
      .join("unit")
      .join("lib")
      .join(exe("unit"))
      .is_file()
  );
}

#[test]
fn output_map_controls_binary_layout() {
  if !have_cc() {
    return;
  }
  let dir = tmp("output_bin");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name out
  language c
  type binary
  link dynamic
  compile { standard c11 }
  output {
    bin "artifacts"
  }
}
"#,
  );
  write(
    &dir.join("src").join("main.c"),
    "int main(void) { return 0; }\n",
  );

  conjure(&dir, &["build"]);
  assert!(
    dir
      .join("artifacts")
      .join("default")
      .join(exe("out"))
      .is_file()
  );
}

#[test]
fn output_map_controls_library_layout() {
  if !have_cc() {
    return;
  }
  let dir = tmp("output_lib");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name outlib
  language c
  type library
  link static
  compile { standard c11 }
  output {
    lib "artifacts-lib"
  }
}
"#,
  );
  write(
    &dir.join("src").join("outlib.c"),
    "int outlib(void) { return 1; }\n",
  );

  conjure(&dir, &["build"]);
  let archive = if cfg!(target_env = "msvc") {
    "outlib.lib"
  } else {
    "liboutlib.a"
  };
  assert!(
    dir
      .join("artifacts-lib")
      .join("default")
      .join(archive)
      .is_file()
  );
}

#[cfg(unix)]
#[test]
fn symlink_binaries_survives_rebuilds() {
  if !have_cc() {
    return;
  }
  let dir = tmp("symlink_rebuild");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name linkme
  language c
  type binary
  link dynamic
  compile { standard c11 }
  output {
    symlink_binaries #true
  }
}
"#,
  );
  write(
    &dir.join("src").join("main.c"),
    "int main(void) { return 0; }\n",
  );

  conjure(&dir, &["build"]);
  let link = dir.join(exe("linkme"));
  assert!(
    link.symlink_metadata().is_ok(),
    "no symlink at {}",
    link.display()
  );
  assert_eq!(
    fs::canonicalize(&link).unwrap(),
    fs::canonicalize(dir.join("bin").join("default").join(exe("linkme")))
      .unwrap()
  );

  // A forced relink must replace the existing link, not fail on it.
  conjure(&dir, &["build", "--force"]);
  assert!(link.symlink_metadata().is_ok());
}

#[cfg(unix)]
#[test]
fn test_targets_inherit_symlink_binaries() {
  if !have_cc() {
    return;
  }
  let dir = tmp("test_symlink");
  write(
    &dir.join("conjure.kdl"),
    r#"project {
  name core
  language c
  type library
  link static
  compile { standard c11 }
  output {
    symlink_binaries "true"
  }
  tests {
    unit { src "src/test_unit.c" }
  }
}
"#,
  );
  write(
    &dir.join("src").join("core.c"),
    "int core(void) { return 42; }\n",
  );
  write(&dir.join("src").join("core.h"), "int core(void);\n");
  write(
    &dir.join("src").join("test_unit.c"),
    "#include \"core.h\"\nint main(void) { return core() == 42 ? 0 : 1; }\n",
  );

  conjure(&dir, &["test"]);
  let link = dir.join(exe("unit"));
  assert!(
    link.symlink_metadata().is_ok(),
    "no symlink at {}",
    link.display()
  );

  // Test binaries always rebuild, so the link must be replaced on re-runs.
  conjure(&dir, &["test"]);
  assert!(link.symlink_metadata().is_ok());
}
