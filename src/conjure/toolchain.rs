//! Compiler and linker dialects.
//!
//! `compile.rs` and `link.rs` speak in semantics ("compile this source", "link
//! these objects into a shared library"); everything that differs between
//! toolchains (flag spelling, object and artifact extensions, the link closure,
//! the archive tool, the build environment) lives behind the [`Driver`] trait
//! here. Adding a toolchain is one `impl Driver` plus a row in [`driver_for`].
//!
//! [`resolve`] picks a driver from the configured `cc`, falling back to a
//! language-appropriate default. It reads nothing but `Compile`, so it is cheap
//! to call per stage; the expensive parts (probing a link closure, capturing the
//! MSVC environment) are memoized.

/***********************************************************************/

use super::proj_parse::{Arch, Compile, Language};
use miette::{Context, IntoDiagnostic, Result};
use std::{ffi::OsString, path::PathBuf, process::Command, sync::OnceLock};

/***********************************************************************/

/// Everything a link needs, stated as semantics; a driver renders it into its
/// own dialect's argv.
pub struct LinkJob<'a> {
  pub shared: bool,
  pub static_flag: Option<String>,
  pub prefix: &'a [String],
  pub objects: &'a [PathBuf],
  pub libs: &'a [PathBuf],
  pub extra_libs: &'a [String],
  pub flags: &'a [String],
  pub out: &'a str,
}

/// One toolchain's dialect.
pub trait Driver {
  // Naming and compile
  fn obj_ext(&self) -> &'static str;
  fn std_args(&self, standard: &str) -> Vec<String>;
  fn include_arg(&self, dir: &str) -> String;
  fn default_c_flags(&self, language: &Language) -> Vec<String>;
  fn compile_tail(&self, src: &str, obj: &str) -> Vec<String>;
  fn pic_flag(&self) -> Option<String>;
  fn arch_flags(&self) -> Vec<String>;

  // Linkage
  fn static_flag(&self) -> Option<String>;
  fn supports_raw_link(&self) -> bool;

  // Link
  fn driver_link_args(&self, job: &LinkJob) -> Vec<String>;
  fn is_driver_flag(&self, f: &str) -> bool;
  fn translate_flag(&self, f: &str) -> Option<Vec<String>>;
  fn needs_driver(&self, f: &str) -> bool;
  fn probe_closure(
    &self,
    cc: &[String],
    extra: &[&str],
  ) -> Result<(Vec<String>, Vec<String>)>;
  fn default_linker(&self, cc: &[String]) -> Result<String>;
  fn raw_link_args(&self, cc: &[String], job: &LinkJob) -> Result<Vec<String>>;

  // Archive
  fn static_archive_name(&self, base: &str) -> String;
  fn archive(&self, out: &str, objects: &[&str]) -> Vec<(String, Vec<String>)>;

  // Environment
  fn env(&self) -> &'static [(OsString, OsString)];
}

/// Shell-like word split for `-###` output: handles `"`/`'` quoting and
/// backslash escapes, which the driver emits around arguments containing
/// spaces.
fn words(line: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut cur = String::new();
  let mut quote: Option<char> = None;
  let mut chars = line.chars().peekable();

  while let Some(c) = chars.next() {
    match quote {
      Some(q) if c == q => quote = None,
      Some(_) if c == '\\' => {
        if let Some(n) = chars.next() {
          cur.push(n);
        }
      }
      Some(_) => cur.push(c),
      None if c == '"' || c == '\'' => quote = Some(c),
      None if c == '\\' => {
        if let Some(n) = chars.next() {
          cur.push(n);
        }
      }
      None if c.is_whitespace() => {
        if !cur.is_empty() {
          out.push(std::mem::take(&mut cur));
        }
      }
      None => cur.push(c),
    }
  }

  if !cur.is_empty() {
    out.push(cur);
  }

  out
}

/// Parse the last linker (or `collect2`) command line out of `cc -###` output
/// into a closure `(prefix, suffix)` split at the dummy input anchor.
///
/// `gcc` wraps the real `ld` in `collect2`; the args after the wrapper are the
/// link closure. Driver-only plumbing (`-o`, `-plugin*`, `-pass-through*`) is
/// dropped, and the split leaves an insertion point for the caller's own
/// objects and libraries in the position the driver would have placed them.
fn parse_link_line(text: &str) -> Result<(Vec<String>, Vec<String>)> {
  let mut link_line = None;
  for line in text.lines().rev() {
    let toks = words(line);
    let prog = toks
      .first()
      .and_then(|t| t.rsplit(['/', '\\']).next())
      .map(|s| s.strip_suffix(".exe").unwrap_or(s));

    if matches!(
      prog,
      Some(
        "ld"
          | "ld.gold"
          | "ld.lld"
          | "lld"
          | "mold"
          | "gold"
          | "collect2"
          | "link"
          | "lld-link"
      )
    ) {
      link_line = Some(toks);
      break;
    }
  }

  let mut toks = link_line.ok_or_else(|| {
    miette::miette!("No linker invocation found in `-###` output")
  })?;
  toks.remove(0);

  let mut i = 0;
  while i < toks.len() {
    if matches!(
      toks[i].as_str(),
      "-o" | "-plugin" | "-plugin-opt" | "-pass-through"
    ) {
      toks.remove(i);
      if i < toks.len() && !toks[i].starts_with('-') {
        toks.remove(i);
      }
    } else if toks[i].starts_with("-plugin-opt=")
      || toks[i].starts_with("-plugin=")
      || toks[i].starts_with("-pass-through=")
    {
      toks.remove(i);
    } else {
      i += 1;
    }
  }

  let anchor = toks.iter().position(|t| t == "/dev/null").ok_or_else(|| {
    miette::miette!("Failed to locate the input anchor in `-###` output")
  })?;
  let suffix = toks.split_off(anchor + 1);
  toks.remove(anchor);
  Ok((toks, suffix))
}

#[derive(Debug, Default)]
pub struct Gnu {
  arch: Arch,
}

impl Gnu {
  /// Map driver-only tokens (pkg-config output, user `ld_flags`) to raw linker
  /// form. Tokens the linker understands directly pass through unchanged.
  fn translated(&self, tokens: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = tokens.iter().peekable();
    while let Some(t) = it.next() {
      if t == "-Xlinker"
        && let Some(arg) = it.next()
      {
        out.push(arg.clone()); // driver forwards the next token as-is
      } else if let Some(repl) = self.translate_flag(t) {
        out.extend(repl);
      } else {
        out.push(t.clone()); // linker-native: -l, -L, -z, --as-needed, ...
      }
    }
    out
  }
}

impl Driver for Gnu {
  fn obj_ext(&self) -> &'static str {
    "o"
  }

  fn std_args(&self, standard: &str) -> Vec<String> {
    vec![format!("-std={standard}")]
  }

  fn include_arg(&self, dir: &str) -> String {
    format!("-I{dir}")
  }

  fn default_c_flags(&self, _language: &Language) -> Vec<String> {
    vec![]
  }

  fn compile_tail(&self, src: &str, obj: &str) -> Vec<String> {
    vec!["-c".into(), src.into(), "-o".into(), obj.into()]
  }

  fn pic_flag(&self) -> Option<String> {
    Some("-fPIC".into())
  }

  fn arch_flags(&self) -> Vec<String> {
    match self.arch {
      Arch::X86 => vec!["-m32".into()],
      Arch::X86_64 => vec!["-m64".into()],
      Arch::Native | Arch::Arm64 => vec![],
    }
  }

  fn static_flag(&self) -> Option<String> {
    Some("-static".into())
  }

  fn supports_raw_link(&self) -> bool {
    true
  }

  fn driver_link_args(&self, job: &LinkJob) -> Vec<String> {
    let mut argv = job.prefix.to_vec();
    argv.extend(self.arch_flags());
    if let Some(sf) = &job.static_flag
      && (sf != "-static" || !job.flags.iter().any(|f| f == "-static"))
    {
      argv.push(sf.clone());
    }

    argv.extend(
      job
        .objects
        .iter()
        .filter_map(|p| p.to_str().map(String::from)),
    );
    argv.extend(job.libs.iter().filter_map(|p| p.to_str().map(String::from)));
    argv.extend(job.extra_libs.iter().cloned());
    argv.extend(job.flags.iter().cloned());
    if job.shared {
      argv.push(if cfg!(target_os = "macos") {
        "-dynamiclib".into()
      } else {
        "-shared".into()
      });
    }

    argv.push("-o".into());
    argv.push(job.out.into());
    argv
  }

  fn is_driver_flag(&self, f: &str) -> bool {
    f.starts_with("-Wl,")
      || f.starts_with("-Xlinker")
      || f.starts_with("-f")
      || f == "-pthread"
      || f.starts_with("-static-lib")
  }

  fn translate_flag(&self, f: &str) -> Option<Vec<String>> {
    if let Some(rest) = f.strip_prefix("-Wl,") {
      return Some(
        rest
          .split(',')
          .filter(|s| !s.is_empty())
          .map(String::from)
          .collect(),
      );
    }
    match f {
      "-pthread" => Some(vec!["-lpthread".into()]),
      "-rdynamic" => Some(vec!["-export-dynamic".into()]),
      "-s" => Some(vec!["-s".into()]),
      "-m32" => Some(vec!["-m".into(), "elf_i386".into()]),
      "-m64" => Some(vec!["-m".into(), "elf_x86_64".into()]),
      "-mx32" => Some(vec!["-m".into(), "elf32_x86_64".into()]),
      "-Bstatic" => Some(vec!["-Bstatic".into()]),
      "-Bdynamic" => Some(vec!["-Bdynamic".into()]),
      _ => None,
    }
  }

  fn needs_driver(&self, f: &str) -> bool {
    self.is_driver_flag(f)
      && !(f == "-Xlinker" || self.translate_flag(f).is_some())
  }

  fn probe_closure(
    &self,
    cc: &[String],
    extra: &[&str],
  ) -> Result<(Vec<String>, Vec<String>)> {
    let out = Command::new(&cc[0])
      .args(&cc[1..])
      .args(self.arch_flags())
      .args(extra)
      .args(["-###", "-o", "/dev/null", "/dev/null"])
      .output()
      .into_diagnostic()
      .wrap_err(format!("Failed to probe '{}' for its link closure", cc[0]))?;

    let text = String::from_utf8_lossy(&out.stdout).into_owned()
      + &String::from_utf8_lossy(&out.stderr);
    parse_link_line(&text)
  }
  fn default_linker(&self, cc: &[String]) -> Result<String> {
    let out = Command::new(&cc[0])
      .args(&cc[1..])
      .arg("-print-prog-name=ld")
      .output()
      .into_diagnostic()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
  }

  fn raw_link_args(&self, cc: &[String], job: &LinkJob) -> Result<Vec<String>> {
    let mode: &[&str] = if job.shared {
      &["-shared"]
    } else if job.static_flag.is_some() {
      &["-static"]
    } else {
      &[]
    };

    let (prefix, suffix) = self.probe_closure(cc, mode)?;

    let mut argv = vec![job.prefix[0].clone()]; // the linker program
    argv.extend(prefix.iter().cloned());
    argv.extend(
      job
        .objects
        .iter()
        .filter_map(|p| p.to_str().map(String::from)),
    );
    argv.extend(job.libs.iter().filter_map(|p| p.to_str().map(String::from)));
    argv.extend(self.translated(job.extra_libs));
    argv.extend(self.translated(job.flags));
    argv.extend(suffix.iter().cloned());
    argv.push("-o".into());
    argv.push(job.out.to_string());
    Ok(argv)
  }

  fn static_archive_name(&self, base: &str) -> String {
    format!("lib{base}.a")
  }

  fn archive(&self, out: &str, objects: &[&str]) -> Vec<(String, Vec<String>)> {
    let mut ar = vec!["ar".into(), "rcs".into(), out.to_string()];
    ar.extend(objects.iter().map(|o| o.to_string()));
    vec![
      ("Archiving".into(), ar),
      ("Indexing".into(), vec!["ranlib".into(), out.to_string()]),
    ]
  }

  fn env(&self) -> &'static [(OsString, OsString)] {
    &[]
  }
}

/// Split a vcvars `set` dump into `KEY=VALUE` pairs.
fn parse_set(text: &str) -> Vec<(OsString, OsString)> {
  text
    .lines()
    .filter_map(|l| l.split_once('='))
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect()
}

/// Locate Visual Studio via `vswhere` and capture the environment `vcvarsall`
/// sets for `arch`, so `cl`/`link.exe`/`lib.exe` and the SDK headers/libs are
/// reachable from a shell that was not launched as a Developer prompt.
///
/// `vcvarsall.bat <arch>` is run via `cmd /c` from its own directory (no
/// embedded quotes, which `cmd` mangles) and its `set` output parsed.
fn detect_msvc_env(arch: Arch) -> Option<Vec<(OsString, OsString)>> {
  if !cfg!(windows) {
    return None;
  }

  let vshere = ["ProgramFiles(x86)", "ProgramFiles"]
    .iter()
    .filter_map(std::env::var_os)
    .map(|p| {
      PathBuf::from(p)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe")
    })
    .find(|p| p.is_file());

  let out = Command::new(vshere.unwrap_or_else(|| "vswhere.exe".into()))
    .args([
      "-latest",
      "-products",
      "*",
      "-requires",
      "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
      "-property",
      "installationPath",
    ])
    .output()
    .ok()?;

  let install = String::from_utf8_lossy(&out.stdout).trim().to_string();
  if install.is_empty() {
    return None;
  }

  let build_dir = PathBuf::from(&install)
    .join("VC")
    .join("Auxiliary")
    .join("Build");
  let vcvarsall = build_dir.join("vcvarsall.bat");
  if !vcvarsall.is_file() {
    return None;
  }

  let target = match arch {
    Arch::X86 => "x86",
    Arch::Arm64 => "arm64",
    Arch::Native | Arch::X86_64 => "x64",
  };

  let out = Command::new("cmd")
    .args(["/c", "vcvarsall.bat", target, "&&", "set"])
    .current_dir(&build_dir)
    .output()
    .ok()?;

  let vars = parse_set(&String::from_utf8_lossy(&out.stdout));
  if std::env::var_os("CONJURE_DEBUG").is_some() {
    eprintln!(
      "Conjure: Ran cmd /c vcvarsall.bat {target} && set in {}",
      build_dir.display()
    );
    eprintln!("Conjure: MSVC install = {install}");
    eprintln!("Conjure: {} env vars captured", vars.len());
    if let Some((_, path)) =
      vars.iter().find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
    {
      let p = path.to_string_lossy().to_lowercase();
      eprintln!(
        "Conjure: captured PATH has cl.exe dir = {}",
        p.contains("vc\\tools") || p.contains("vc/tools")
      );
    } else {
      eprintln!("Conjure: No PATH captured from vcvars!");
    }
  }

  Some(vars)
}

fn msvc_env(arch: Arch) -> &'static [(OsString, OsString)] {
  static ENVS: [OnceLock<Vec<(OsString, OsString)>>; 3] =
    [OnceLock::new(), OnceLock::new(), OnceLock::new()];
  let arch = if matches!(arch, Arch::Native) {
    Arch::X86_64
  } else {
    arch
  };
  let slot = match arch {
    Arch::X86_64 => 0,
    Arch::X86 => 1,
    Arch::Arm64 => 2,
    Arch::Native => unreachable!(),
  };
  ENVS[slot].get_or_init(|| {
    let env = detect_msvc_env(arch);
    if env.is_none() {
      eprintln!(
        "Conjure: Could not locate the MSVC environment for {arch:?}; \
         use a Developer prompt or set `cc`/`linker` explicitly"
      );
    }
    env.unwrap_or_default()
  })
}

#[derive(Debug, Default)]
pub struct Microsoft {
  arch: Arch,
}

impl Driver for Microsoft {
  fn obj_ext(&self) -> &'static str {
    "obj"
  }

  fn std_args(&self, standard: &str) -> Vec<String> {
    vec![format!("/std:{standard}")]
  }

  fn include_arg(&self, dir: &str) -> String {
    format!("/I{dir}")
  }

  fn default_c_flags(&self, language: &Language) -> Vec<String> {
    let mut flags = vec!["/nologo".into()];
    if matches!(language, Language::Cpp) {
      flags.push("/EHsc".into());
    }
    flags
  }

  fn compile_tail(&self, src: &str, obj: &str) -> Vec<String> {
    vec!["/c".into(), src.into(), format!("/Fo:{obj}")]
  }

  fn pic_flag(&self) -> Option<String> {
    None
  }

  fn arch_flags(&self) -> Vec<String> {
    vec![] // vcvarsall selects the target; cl/link take no -m
  }

  fn static_flag(&self) -> Option<String> {
    Some("/MT".into())
  }

  fn supports_raw_link(&self) -> bool {
    true
  }

  fn driver_link_args(&self, job: &LinkJob) -> Vec<String> {
    let mut argv = job.prefix.to_vec();
    argv.push("/nologo".into());
    if job.shared {
      argv.push("/LD".into());
    }

    if let Some(sf) = &job.static_flag {
      argv.push(sf.clone());
    }

    argv.extend(
      job
        .objects
        .iter()
        .filter_map(|p| p.to_str().map(String::from)),
    );
    argv.extend(job.libs.iter().filter_map(|p| p.to_str().map(String::from)));
    argv.extend(job.extra_libs.iter().cloned());
    argv.push(format!("/Fe:{}", job.out));
    if !job.flags.is_empty() {
      argv.push("/link".into());
      argv.extend(job.flags.iter().cloned());
    }

    argv
  }

  fn is_driver_flag(&self, _f: &str) -> bool {
    false // no raw default; flags are cl/link.exe syntax already
  }

  fn translate_flag(&self, _f: &str) -> Option<Vec<String>> {
    None
  }

  fn needs_driver(&self, _f: &str) -> bool {
    false
  }

  fn probe_closure(
    &self,
    _cc: &[String],
    _extra: &[&str],
  ) -> Result<(Vec<String>, Vec<String>)> {
    Ok((vec![], vec![])) // msvc objects carry /DEFAULTLIB; no closure needed
  }

  fn default_linker(&self, _cc: &[String]) -> Result<String> {
    Ok("link.exe".into())
  }

  fn raw_link_args(
    &self,
    _cc: &[String],
    job: &LinkJob,
  ) -> Result<Vec<String>> {
    let mut argv = vec![job.prefix[0].clone(), "/nologo".into()];
    if job.shared {
      argv.push("/DLL".into());
    }
    argv.extend(
      job
        .objects
        .iter()
        .filter_map(|p| p.to_str().map(String::from)),
    );
    argv.extend(job.libs.iter().filter_map(|p| p.to_str().map(String::from)));
    argv.extend(job.extra_libs.iter().cloned());
    argv.push(format!("/OUT:{}", job.out));
    argv.extend(job.flags.iter().cloned());
    Ok(argv)
  }

  fn static_archive_name(&self, base: &str) -> String {
    format!("{base}.lib")
  }
  fn archive(&self, out: &str, objects: &[&str]) -> Vec<(String, Vec<String>)> {
    let mut args =
      vec!["lib.exe".into(), "/nologo".into(), format!("/OUT:{out}")];
    args.extend(objects.iter().map(|o| o.to_string()));
    vec![("Archiving".into(), args)]
  }

  fn env(&self) -> &'static [(OsString, OsString)] {
    msvc_env(self.arch)
  }
}

/// A resolved compiler plus its dialect, with forwarding methods so callers
/// never name a specific driver.
pub struct Toolchain {
  pub cc: Vec<String>,
  pub driver: Box<dyn Driver>,
}

impl Toolchain {
  pub fn obj_ext(&self) -> &'static str {
    self.driver.obj_ext()
  }

  pub fn std_args(&self, standard: &str) -> Vec<String> {
    self.driver.std_args(standard)
  }

  pub fn include_arg(&self, dir: &str) -> String {
    self.driver.include_arg(dir)
  }

  pub fn default_c_flags(&self, language: &Language) -> Vec<String> {
    self.driver.default_c_flags(language)
  }

  pub fn compile_tail(&self, src: &str, obj: &str) -> Vec<String> {
    self.driver.compile_tail(src, obj)
  }

  pub fn pic_flag(&self) -> Option<String> {
    self.driver.pic_flag()
  }

  pub fn arch_flags(&self) -> Vec<String> {
    self.driver.arch_flags()
  }

  pub fn static_flag(&self) -> Option<String> {
    self.driver.static_flag()
  }

  pub fn supports_raw_link(&self) -> bool {
    self.driver.supports_raw_link()
  }

  pub fn driver_link_args(&self, job: &LinkJob) -> Vec<String> {
    self.driver.driver_link_args(job)
  }

  pub fn needs_driver(&self, f: &str) -> bool {
    self.driver.needs_driver(f)
  }

  pub fn default_linker(&self) -> Result<String> {
    self.driver.default_linker(self.cc.as_slice())
  }

  pub fn raw_link_args(&self, job: &LinkJob) -> Result<Vec<String>> {
    self.driver.raw_link_args(self.cc.as_slice(), job)
  }

  pub fn static_archive_name(&self, base: &str) -> String {
    self.driver.static_archive_name(base)
  }

  pub fn archive(
    &self,
    out: &str,
    objects: &[&str],
  ) -> Vec<(String, Vec<String>)> {
    self.driver.archive(out, objects)
  }

  pub fn env(&self) -> &'static [(OsString, OsString)] {
    self.driver.env()
  }
}

fn split(tool: &str) -> Vec<String> {
  tool.split_whitespace().map(|s| s.to_string()).collect()
}

/// Basename of the actual tool, unwrapping a transparent compiler wrapper
/// (`ccache`, `sccache`, ...) once.
fn real_tool(argv: &[String]) -> &str {
  let mut name = argv.first().map(String::as_str).unwrap_or("cc");
  if matches!(name, "ccache" | "sccache" | "distcc" | "icecc") {
    name = argv.get(1).map(String::as_str).unwrap_or("cc");
  }
  name.rsplit(['/', '\\']).next().unwrap_or(name)
}

// Extending conjure to a new tool = one row here.
fn driver_for(name: &str, arch: Arch) -> Box<dyn Driver> {
  match name {
    "cl" | "clang-cl" | "icx" | "icl" => Box::new(Microsoft { arch }),
    _ => Box::new(Gnu { arch }),
  }
}

/// The compiler to use when none is configured. MSVC is only chosen on Windows
/// outside an MSYS/MinGW shell and only when Visual Studio is actually
/// locatable, so a machine without VS falls back to `cc`/`c++`.
fn default_compiler(language: &Language) -> &'static str {
  static MSVC: OnceLock<bool> = OnceLock::new();
  let msvc = *MSVC.get_or_init(|| {
    cfg!(windows)
      && std::env::var_os("MSYSTEM").is_none() // MSYS/MinGW shell -> gnu
      && !msvc_env(Arch::Native).is_empty() // VS locatable via vswhere
  });

  if msvc {
    "cl"
  } else {
    match language {
      Language::C => "cc",
      Language::Cpp => "c++",
    }
  }
}

/// Resolve a [`Toolchain`] from `compile`'s `cc`/`arch`, defaulting the compiler
/// by language when unset.
pub fn resolve(compile: &Compile, language: &Language) -> Toolchain {
  let arch = compile.arch.unwrap_or_default();
  let default = default_compiler(language);
  let cc = compile
    .cc
    .as_deref()
    .filter(|s| !s.trim().is_empty())
    .map(split)
    .unwrap_or_else(|| vec![default.to_string()]);
  let name = real_tool(&cc).to_string();

  Toolchain {
    cc,
    driver: driver_for(&name, arch),
  }
}

#[cfg(test)]
mod tests {
  use super::{
    Arch, Compile, Driver, Gnu, Language, LinkJob, Microsoft, Toolchain,
    driver_for, parse_link_line, parse_set, real_tool, resolve, words,
  };
  use std::path::PathBuf;

  #[cfg(windows)]
  use super::default_compiler;

  #[test]
  fn words_handles_quotes_and_escapes() {
    assert_eq!(
      words("-a \"b c\" 'd e' f\\ g"),
      vec!["-a", "b c", "d e", "f g"]
    );
    assert_eq!(words("plain"), vec!["plain"]);
    assert_eq!(words(""), Vec::<String>::new());
  }

  #[test]
  fn parse_link_line_splits_closure_at_the_input_anchor() {
    let text = "Using built-in specs.\n\
gcc version 15.3.0\n\
/usr/lib/collect2 -plugin /x/plugin.so -plugin-opt=-pass-through=-lgcc \"-plugin-opt=-z relro\" \
-o /dev/null --eh-frame-hdr -m elf_x86_64 -pie /lib64/Scrt1.o /lib64/crti.o /lib64/crtbeginS.o \
-L/lib64 -lc /dev/null -lgcc /lib64/crtendS.o /lib64/crtn.o";
    let (prefix, suffix) = parse_link_line(text).unwrap();
    assert_eq!(
      prefix,
      vec![
        "--eh-frame-hdr",
        "-m",
        "elf_x86_64",
        "-pie",
        "/lib64/Scrt1.o",
        "/lib64/crti.o",
        "/lib64/crtbeginS.o",
        "-L/lib64",
        "-lc",
      ]
    );
    assert_eq!(suffix, vec!["-lgcc", "/lib64/crtendS.o", "/lib64/crtn.o"]);
  }

  #[test]
  fn gnu_archive_is_ar_plus_ranlib() {
    let tc = Toolchain::gnu(vec!["gcc".into()]);
    assert_eq!(tc.static_archive_name("x"), "libx.a");
    let steps = tc.archive("libx.a", &["a.o", "b.o"]);
    assert_eq!(steps[0].0, "Archiving");
    assert_eq!(steps[0].1, vec!["ar", "rcs", "libx.a", "a.o", "b.o"]);
    assert_eq!(steps[1].1, vec!["ranlib", "libx.a"]);
  }

  #[test]
  fn microsoft_archive_uses_lib_exe() {
    let tc = Toolchain::microsoft(vec!["cl".into()]);
    assert_eq!(tc.static_archive_name("x"), "x.lib");
    let steps = tc.archive("x.lib", &["a.obj"]);
    assert_eq!(steps.len(), 1);
    assert_eq!(
      steps[0].1,
      vec!["lib.exe", "/nologo", "/OUT:x.lib", "a.obj"]
    );
  }

  #[test]
  fn parse_set_splits_key_values() {
    let env = parse_set("PATH=C:\\x\r\nLIB=d=1;e\r\nbogus\r\n");
    assert!(env.contains(&("PATH".into(), "C:\\x".into())));
    assert!(env.contains(&("LIB".into(), "d=1;e".into())));
    assert_eq!(env.len(), 2);
  }

  #[test]
  fn msvc_cpp_gets_ehsc() {
    let tc = Toolchain::microsoft(vec!["cl".into()]);
    assert_eq!(tc.default_c_flags(&Language::Cpp), vec!["/nologo", "/EHsc"]);
    assert_eq!(tc.default_c_flags(&Language::C), vec!["/nologo"]);
  }

  #[test]
  fn microsoft_default_linker_is_link_exe() {
    assert_eq!(
      Toolchain::microsoft(vec!["cl".into()])
        .default_linker()
        .unwrap(),
      "link.exe"
    );
    assert_eq!(
      Toolchain::microsoft(vec!["clang-cl".into()])
        .default_linker()
        .unwrap(),
      "link.exe"
    );
  }

  #[test]
  fn gnu_arch_flags_per_target() {
    let tc = |arch| Toolchain {
      cc: vec!["gcc".into()],
      driver: Box::new(Gnu { arch }),
    };
    assert_eq!(tc(Arch::X86).arch_flags(), vec!["-m32"]);
    assert_eq!(tc(Arch::X86_64).arch_flags(), vec!["-m64"]);
    assert!(tc(Arch::Native).arch_flags().is_empty());
    assert!(tc(Arch::Arm64).arch_flags().is_empty());
  }

  #[test]
  fn microsoft_arch_flags_always_empty() {
    let tc = Toolchain {
      cc: vec!["cl".into()],
      driver: Box::new(Microsoft { arch: Arch::X86 }),
    };
    assert!(tc.arch_flags().is_empty());
  }

  fn gnu(arch: Arch) -> Gnu {
    Gnu { arch }
  }
  fn ms(arch: Arch) -> Microsoft {
    Microsoft { arch }
  }

  #[test]
  fn words_handles_edge_cases() {
    assert_eq!(words("a   b"), vec!["a", "b"]);
    assert_eq!(words("a b\\"), vec!["a", "b"]); // trailing backslash
    assert_eq!(words("\"a b"), vec!["a b"]); // unterminated quote
    assert_eq!(words(r#""a\"b""#), vec!["a\"b"]); // escape inside quotes
    assert_eq!(words("\"\""), Vec::<String>::new()); // empty quoted token
  }

  #[test]
  fn parse_link_line_strips_plumbing() {
    assert!(parse_link_line("gcc version 15\nnothing here").is_err());

    let line = "collect2 --eh-frame-hdr -m elf_x86_64 -o /dev/null \
                -plugin /x/plugin.so -plugin-opt=-pass-through=-lgcc \
                /dev/null -lc -lgcc";
    let (prefix, suffix) = parse_link_line(line).unwrap();
    assert_eq!(prefix, vec!["--eh-frame-hdr", "-m", "elf_x86_64"]);
    assert_eq!(suffix, vec!["-lc", "-lgcc"]);
    assert!(prefix.iter().chain(&suffix).all(|t| !t.contains("plugin")));
  }

  #[test]
  fn compile_side_methods_per_dialect() {
    let g = gnu(Arch::X86_64);
    assert_eq!(g.obj_ext(), "o");
    assert_eq!(g.std_args("c11"), vec!["-std=c11"]);
    assert_eq!(g.include_arg("inc"), "-Iinc");
    assert_eq!(g.compile_tail("a.c", "a.o"), vec!["-c", "a.c", "-o", "a.o"]);
    assert_eq!(g.pic_flag().as_deref(), Some("-fPIC"));
    assert!(g.default_c_flags(&Language::C).is_empty());

    let m = ms(Arch::X86_64);
    assert_eq!(m.obj_ext(), "obj");
    assert_eq!(m.std_args("c17"), vec!["/std:c17"]);
    assert_eq!(m.include_arg("inc"), "/Iinc");
    assert_eq!(
      m.compile_tail("a.c", "a.obj"),
      vec!["/c", "a.c", "/Fo:a.obj"]
    );
    assert_eq!(m.pic_flag(), None);
    assert!(m.arch_flags().is_empty());
    assert_eq!(m.default_c_flags(&Language::Cpp), vec!["/nologo", "/EHsc"]);
  }

  #[test]
  fn gnu_flag_translation() {
    let g = gnu(Arch::Native);
    assert_eq!(
      g.translate_flag("-Wl,-z,relro"),
      Some(vec!["-z".into(), "relro".into()])
    );
    assert_eq!(g.translate_flag("-pthread"), Some(vec!["-lpthread".into()]));
    assert_eq!(
      g.translate_flag("-rdynamic"),
      Some(vec!["-export-dynamic".into()])
    );
    assert_eq!(
      g.translate_flag("-m32"),
      Some(vec!["-m".into(), "elf_i386".into()])
    );
    assert_eq!(
      g.translate_flag("-m64"),
      Some(vec!["-m".into(), "elf_x86_64".into()])
    );
    assert_eq!(
      g.translate_flag("-mx32"),
      Some(vec!["-m".into(), "elf32_x86_64".into()])
    );
    assert_eq!(g.translate_flag("-Bstatic"), Some(vec!["-Bstatic".into()]));
    assert_eq!(
      g.translate_flag("-Bdynamic"),
      Some(vec!["-Bdynamic".into()])
    );
    assert_eq!(g.translate_flag("-lm"), None);

    assert!(!g.needs_driver("-Wl,x"));
    assert!(!g.needs_driver("-Wl,-z,relro"));
    assert!(!g.needs_driver("-Xlinker"));
    assert!(!g.needs_driver("-pthread"));
    assert!(g.needs_driver("-fuse-ld=mold"));
    assert!(g.needs_driver("-static-libgcc"));
    assert!(!g.needs_driver("-lm"));

    let toks = vec![
      "-Xlinker".to_string(),
      "--as-needed".to_string(),
      "-Wl,-z,relro".to_string(),
      "-lm".to_string(),
    ];
    assert_eq!(
      g.translated(&toks),
      vec!["--as-needed", "-z", "relro", "-lm"]
    );
  }

  #[test]
  fn gnu_driver_link_args_matrix() {
    let prefix = vec!["gcc".to_string()];
    let objects = vec![PathBuf::from("a.o")];
    let libs = vec![PathBuf::from("./libx.a")];
    let extra = vec!["-lm".to_string()];
    let flags = vec!["-O2".to_string()];
    let flags_static = vec!["-static".to_string()];

    let job = LinkJob {
      shared: false,
      static_flag: None,
      prefix: &prefix,
      objects: &objects,
      libs: &libs,
      extra_libs: &extra,
      flags: &flags,
      out: "prog",
    };
    assert_eq!(
      gnu(Arch::Native).driver_link_args(&job),
      vec!["gcc", "a.o", "./libx.a", "-lm", "-O2", "-o", "prog"]
    );

    let job = LinkJob {
      shared: true,
      ..job
    };
    let argv = gnu(Arch::Native).driver_link_args(&job);
    assert!(argv.iter().any(|a| a == "-shared" || a == "-dynamiclib"));

    let job = LinkJob {
      shared: false,
      static_flag: Some("-static".into()),
      prefix: &prefix,
      objects: &objects,
      libs: &libs,
      extra_libs: &extra,
      flags: &flags,
      out: "prog",
    };
    let argv = gnu(Arch::Native).driver_link_args(&job);
    assert_eq!(argv.iter().filter(|a| *a == "-static").count(), 1);

    let job = LinkJob {
      flags: &flags_static,
      ..job
    };
    let argv = gnu(Arch::Native).driver_link_args(&job);
    assert_eq!(argv.iter().filter(|a| *a == "-static").count(), 1);
  }

  #[test]
  fn msvc_link_args_matrix() {
    let prefix = vec!["cl".to_string()];
    let objects = vec![PathBuf::from("a.obj")];
    let libs = vec![PathBuf::from("x.lib")];
    let extra = vec!["user32.lib".to_string()];
    let flags = vec!["/DEBUG".to_string()];
    let empty: Vec<String> = vec![];

    let job = LinkJob {
      shared: false,
      static_flag: Some("/MT".into()),
      prefix: &prefix,
      objects: &objects,
      libs: &libs,
      extra_libs: &extra,
      flags: &flags,
      out: "prog.exe",
    };
    let argv = ms(Arch::X86_64).driver_link_args(&job);
    for want in ["/nologo", "/MT", "/Fe:prog.exe", "/link", "/DEBUG"] {
      assert!(argv.contains(&want.to_string()), "missing {want}");
    }
    assert!(!argv.contains(&"/LD".to_string()));

    let job = LinkJob {
      shared: true,
      static_flag: None,
      flags: &empty,
      ..job
    };
    let argv = ms(Arch::X86_64).driver_link_args(&job);
    assert!(argv.contains(&"/LD".to_string()));
    assert!(!argv.contains(&"/link".to_string()));

    let raw = ms(Arch::X86_64).raw_link_args(&prefix, &job).unwrap();
    assert_eq!(raw[0], "cl");
    assert!(raw.contains(&"/DLL".to_string()));
    assert!(raw.contains(&"/OUT:prog.exe".to_string()));
  }

  #[test]
  fn resolve_and_driver_selection() {
    assert_eq!(real_tool(&["ccache".into(), "gcc".into()]), "gcc");
    assert_eq!(real_tool(&["/usr/bin/cl".into()]), "cl");
    assert_eq!(real_tool(&[]), "cc");
    assert_eq!(driver_for("icx", Arch::Native).obj_ext(), "obj");
    assert_eq!(driver_for("gcc", Arch::Native).obj_ext(), "o");

    let c = Compile {
      cc: Some("clang-cl".into()),
      ..Default::default()
    };
    assert_eq!(resolve(&c, &Language::C).obj_ext(), "obj");

    let c = Compile {
      cc: Some("ccache gcc".into()),
      ..Default::default()
    };
    let tc = resolve(&c, &Language::C);
    assert_eq!(tc.cc, vec!["ccache", "gcc"]);
    assert_eq!(tc.obj_ext(), "o");
  }

  #[cfg(all(windows, target_env = "msvc"))]
  #[test]
  fn msvc_env_is_captured_on_windows() {
    assert!(
      !Microsoft { arch: Arch::Native }.env().is_empty(),
      "vswhere found no Visual Studio; the MSVC driver cannot work"
    );
  }

  #[cfg(all(windows, target_env = "msvc"))]
  #[test]
  fn default_compiler_is_msvc_when_visual_studio_is_present() {
    assert_eq!(default_compiler(&Language::C), "cl");
  }
}
