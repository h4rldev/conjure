//! The `conjure.kdl` schema: parsing, validation, and profile resolution.
//!
//! [`Project`] mirrors the manifest. Parsing is serde-driven over KDL, with
//! custom deserializers for the enums written as bare tokens and for the project
//! name (slugified on read). [`Project::from_str`] parses and then validates
//! dependencies; failure modes are enumerated in [`Error`].
//!
//! Profiles are not applied at parse time. [`Project::with_profile`] produces the
//! effective project for a build by folding a [`Profile`] over the base, so the
//! compile/link/fingerprint/deps code all works against a plain [`Project`] and
//! never branches on profiles itself.

/***********************************************************************/

use miette::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path, string::String};

/***********************************************************************/

#[derive(Deserialize, Default, Clone, Debug, Eq, PartialEq)]
#[serde(try_from = "String")]
pub enum Language {
  #[default]
  C,
  Cpp,
}

impl TryFrom<String> for Language {
  type Error = String;
  fn try_from(s: String) -> Result<Self, Self::Error> {
    match s.to_ascii_lowercase().as_str() {
      "c" => Ok(Self::C),
      "c++" | "cpp" => Ok(Self::Cpp),
      other => Err(format!("unknown language: {other}")),
    }
  }
}

#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum ProjectType {
  #[default]
  Binary,
  Library,
}

impl TryFrom<String> for ProjectType {
  type Error = String;
  fn try_from(s: String) -> Result<Self, Self::Error> {
    match s.to_ascii_lowercase().as_str() {
      "binary" => Ok(Self::Binary),
      "library" => Ok(Self::Library),
      other => Err(format!("Unknown project type: {other:?}")),
    }
  }
}

/// Static versus dynamic linkage. Lives apart from [`ProjectType`] because it is
/// a build variant (profile-overridable), not part of what the project *is*.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum Linkage {
  #[default]
  Dynamic,
  Static,
}

impl TryFrom<String> for Linkage {
  type Error = String;
  fn try_from(s: String) -> Result<Self, Self::Error> {
    match s.to_ascii_lowercase().as_str() {
      "dynamic" => Ok(Self::Dynamic),
      "static" => Ok(Self::Static),
      other => Err(format!("Unknown linkage: {other:?}")),
    }
  }
}

#[derive(Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum Arch {
  #[default]
  Native,
  X86,
  X86_64,
  Arm64,
}

impl TryFrom<String> for Arch {
  type Error = String;
  fn try_from(s: String) -> Result<Self, Self::Error> {
    match s.to_ascii_lowercase().as_str() {
      "native" => Ok(Self::Native),
      "x86" | "i686" | "i386" | "32" => Ok(Self::X86),
      "x64" | "x86_64" | "amd64" | "64" => Ok(Self::X86_64),
      "arm64" | "aarch64" => Ok(Self::Arm64),
      other => Err(format!("Unknown arch: {other:?}")),
    }
  }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum Transport {
  Ssh,
  Https,
}

impl TryFrom<String> for Transport {
  type Error = String;
  fn try_from(s: String) -> Result<Self, Self::Error> {
    match s.to_ascii_lowercase().as_str() {
      "ssh" => Ok(Self::Ssh),
      "https" => Ok(Self::Https),
      other => Err(format!("unknown transport: {other}")),
    }
  }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(try_from = "Vec<String>")]
pub enum Remote {
  Codeberg(String),
  GitHub(String),
  BitBucket(String),
  Git(String),
}

impl TryFrom<Vec<String>> for Remote {
  type Error = String;
  fn try_from(mut v: Vec<String>) -> Result<Self, Self::Error> {
    if v.len() < 2 {
      return Err(format!("remote must have at least 2 elements got: {v:?}"));
    }

    let path = v.remove(1);
    match v.remove(0).to_ascii_lowercase().as_str() {
      "codeberg" => Ok(Self::Codeberg(path)),
      "github" => Ok(Self::GitHub(path)),
      "bitbucket" => Ok(Self::BitBucket(path)),
      "git" => Ok(Self::Git(path)),
      other => Err(format!("unknown remote: {other}")),
    }
  }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(try_from = "Vec<String>")]
pub enum BuildSystem {
  Conjure(Option<String>),
  Make(Option<String>),
  CMake(Option<String>),
  Autotools(Option<String>),
  Meson(Option<String>),
  Ninja(Option<String>),
  Xmake(Option<String>),
  Just(String),
  Custom(String),
}

impl std::fmt::Display for BuildSystem {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      BuildSystem::Conjure(_target) => {
        write!(f, "conjure")
      }
      BuildSystem::Make(_target) => {
        write!(f, "make")
      }
      BuildSystem::CMake(_target) => {
        write!(f, "cmake")
      }
      BuildSystem::Autotools(_target) => {
        write!(f, "autotools")
      }
      BuildSystem::Meson(_target) => {
        write!(f, "meson")
      }
      BuildSystem::Ninja(_target) => {
        write!(f, "ninja")
      }
      BuildSystem::Xmake(_target) => {
        write!(f, "xmake")
      }
      BuildSystem::Just(_target) => write!(f, "just"),
      BuildSystem::Custom(cmd) => {
        let cmd_executable = cmd.split_whitespace().next().unwrap();
        write!(f, "custom ({})", cmd_executable)
      }
    }
  }
}

impl TryFrom<Vec<String>> for BuildSystem {
  type Error = String;
  fn try_from(v: Vec<String>) -> Result<Self, Self::Error> {
    let first = v.first().map(|s| s.to_ascii_lowercase());
    match first.as_deref() {
      Some("conjure") => Ok(Self::Conjure(v.get(1).cloned())),
      Some("make") => Ok(Self::Make(v.get(1).cloned())),
      Some("cmake") => Ok(Self::CMake(v.get(1).cloned())),
      Some("autotools") => Ok(Self::Autotools(v.get(1).cloned())),
      Some("meson") => Ok(Self::Meson(v.get(1).cloned())),
      Some("ninja") => Ok(Self::Ninja(v.get(1).cloned())),
      Some("xmake") => Ok(Self::Xmake(v.get(1).cloned())),
      Some("just") => Ok(Self::Just(
        v.get(1)
          .cloned()
          .filter(|f| !f.is_empty())
          .unwrap_or("build".to_string())
          .to_string(),
      )),
      _ => Ok(Self::Custom(v.join(" "))),
    }
  }
}

/// A list of flags with merge semantics: `Append` adds to the base list, while
/// `Replace` discards it. The distinction only matters for profile merges; a
/// bare `c_flags -O2` parses as `Append`.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(try_from = "Vec<String>")]
pub enum Flags {
  Append(Vec<String>),
  Replace(Vec<String>),
}

impl Default for Flags {
  fn default() -> Self {
    Self::Append(Vec::new())
  }
}

impl Flags {
  /// The contained flags, empty strings dropped (they appear when a list is
  /// written as `""` in KDL).
  pub fn list(&self) -> Vec<String> {
    let v = match self {
      Self::Append(v) => v,
      Self::Replace(v) => v,
    };

    v.iter().filter(|s| !s.is_empty()).cloned().collect()
  }
}

impl TryFrom<Vec<String>> for Flags {
  type Error = String;
  fn try_from(mut v: Vec<String>) -> Result<Self, Self::Error> {
    let first = v.first().map(|s| s.to_ascii_lowercase());

    match first.as_deref() {
      Some("append") | Some("add") | Some("+") => {
        v.remove(0);
        Ok(Self::Append(v))
      }
      Some("replace") | Some("set") | Some("=") => {
        v.remove(0);
        Ok(Self::Replace(v))
      }
      _ => Ok(Self::Append(v)),
    }
  }
}

/// Fold a profile's flags over a base set: `Replace` overrides, otherwise the
/// lists concatenate.
fn merge_flags(base: Option<&Flags>, over: Option<&Flags>) -> Option<Flags> {
  match over {
    None => base.cloned(),
    Some(Flags::Replace(v)) => Some(Flags::Replace(v.clone())),
    Some(Flags::Append(v)) => {
      let mut out = base.map(Flags::list).unwrap_or_default();
      out.extend(v.iter().filter(|s| !s.is_empty()).cloned());
      Some(Flags::Append(out))
    }
  }
}

#[derive(Deserialize, Default, Serialize, Clone, Debug)]
pub struct Compile {
  pub cc: Option<String>,
  pub linker: Option<String>,
  pub standard: Option<String>,
  pub include: Option<Flags>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub src: Option<Flags>,
  #[serde(default)]
  pub c_flags: Option<Flags>,
  #[serde(default)]
  pub ld_flags: Option<Flags>,
  #[serde(default)]
  pub threads: Option<usize>,
  #[serde(default)]
  pub arch: Option<Arch>,
}

impl Compile {
  /// Source roots to recurse for compilation, falling back to `["src"]` when
  /// unset or empty.
  pub fn src_roots(&self) -> Vec<String> {
    self
      .src
      .as_ref()
      .map(Flags::list)
      .filter(|v| !v.is_empty())
      .unwrap_or_else(|| vec!["src".to_string()])
  }

  /// Fold a profile's compile settings over these: scalar fields override when
  /// set, flag/src/include lists merge via [`merge_flags`].
  pub fn with_profile(&self, p: &Profile) -> Compile {
    Compile {
      cc: p.cc.clone().or_else(|| self.cc.clone()),
      linker: p.linker.clone().or_else(|| self.linker.clone()),
      standard: p.standard.clone().or_else(|| self.standard.clone()),
      include: merge_flags(self.include.as_ref(), p.include.as_ref()),
      src: merge_flags(self.src.as_ref(), p.src.as_ref()),
      c_flags: merge_flags(self.c_flags.as_ref(), p.c_flags.as_ref()),
      ld_flags: merge_flags(self.ld_flags.as_ref(), p.ld_flags.as_ref()),
      threads: p.threads.or(self.threads),
      arch: p.arch.or(self.arch),
    }
  }
}

#[derive(Deserialize, Clone, Serialize, Debug)]
pub struct Sibling {
  #[serde(rename = "#0")]
  pub path: String,
}

#[derive(Deserialize, Clone, Serialize, Debug)]
pub struct Test {
  #[serde(default)]
  pub src: Option<Flags>,
}

#[derive(Deserialize, Default, Clone, Serialize, Debug)]
pub struct Output {
  pub bin: Option<String>,
  pub lib: Option<String>,
  pub symlink_binaries: Option<bool>,
}

#[derive(Deserialize, Clone, Default, Serialize, Debug)]
pub struct Dependency {
  #[serde(default)]
  pub remote: Option<Remote>,
  #[serde(default)]
  pub local: Option<String>,
  #[serde(default)]
  pub transport: Option<Transport>,
  #[serde(default)]
  pub build: Option<BuildSystem>,
  #[serde(default)]
  pub include: Option<Vec<String>>,
  #[serde(default)]
  pub src: Option<Vec<String>>,
  #[serde(default)]
  pub pkg_config: Option<Vec<String>>,
  #[serde(default)]
  pub r#ref: Option<String>,
}

impl Dependency {
  /// Reject the incompatible combinations: exactly one of `remote`/`local`, and
  /// no `transport` on a local dependency.
  pub fn validate(&self) -> Result<(), String> {
    match (&self.remote, &self.local) {
      (Some(_), Some(_)) => {
        Err("Remote and local cannot be used at the same time".into())
      }
      (None, None) => Err("Remote or local must be used".into()),
      (None, Some(_)) if self.transport.is_some() => {
        Err("Transport cannot be used with local".into())
      }
      _ => Ok(()),
    }
  }
}

#[derive(Deserialize, Clone, Default, Serialize, Debug)]
pub struct Profile {
  #[serde(default, rename = "type")]
  pub ty: Option<ProjectType>,
  #[serde(default)]
  pub link: Option<Linkage>,
  #[serde(default)]
  pub cc: Option<String>,
  #[serde(default)]
  pub linker: Option<String>,
  #[serde(default)]
  pub standard: Option<String>,
  #[serde(default)]
  pub include: Option<Flags>,
  #[serde(default)]
  pub src: Option<Flags>,
  #[serde(default)]
  pub c_flags: Option<Flags>,
  #[serde(default)]
  pub ld_flags: Option<Flags>,
  #[serde(default)]
  pub threads: Option<usize>,
  #[serde(default)]
  pub arch: Option<Arch>,
  #[serde(default)]
  pub tests: Option<HashMap<String, Test>>,
  #[serde(default)]
  pub dependencies: Option<HashMap<String, Dependency>>,
}

/// Lowercase, keep alphanumerics, collapse every other run into a single `-`.
pub fn slugify(input: &str) -> String {
  input
    .to_lowercase()
    .chars()
    .map(|c| if c.is_alphanumeric() { c } else { '-' })
    .collect::<String>()
    .split('-')
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join("-")
}

/// Serde adapter applying [`slugify`] to the project name on read.
fn de_slugify<'de, D>(d: D) -> Result<String, D::Error>
where
  D: serde::Deserializer<'de>,
{
  Ok(slugify(&String::deserialize(d)?))
}

/// Serde adapter defaulting an absent `link` field to [`Linkage::Dynamic`].
fn de_linkage<'de, D>(d: D) -> Result<Linkage, D::Error>
where
  D: serde::Deserializer<'de>,
{
  use serde::de::Error;
  match Option::<Linkage>::deserialize(d) {
    Ok(Some(l)) => Ok(l),
    Ok(None) => Ok(Linkage::Dynamic),
    Err(_) => Err(D::Error::custom("invalid linkage")),
  }
}

/// The project file format.
///
/// # Schema
///
/// ```
/// project {
///   name: string (slugifies in deserialization),
///   language: string (matches any known form of C or C++ such as C, C++, Cpp, etc.)
///   type: string (binary | library)
///   link: string (dynamic | static; default dynamic)
///   license: string
///   authors: [string]
///   description: string
///
///   compile {
///     src: [string] (source roots: directories to recurse or exact files; defaults to ["src"])
///     cc: string (path to the compiler, defaults to cc)
///     linker: string (path to the linker, defaults to the compiler)
///     standard: string (e.g. "c11", "gnu17", "c++20")
///     include: [string] (include dirs, each becomes -I<dir>)
///     c_flags: [string]
///     ld_flags: [string]
///     threads: int (parallelism for compiling and dep builds)
///     arch: string (native | x86 | x64 | arm64; default native)
///   }
///
///   siblings {
///     name path: string (a sibling conjure project with its own conjure.kdl, built
///     alongside this one in the same `conjure build`)
///   }
///
///   tests {
///     name {
///       src: [string] (source roots for this test binary; the project must be
///       `type library` since the test links it)
///     }
///   }
///
///   profiles {
///     name: string {
///       type, link, cc, linker, standard, arch: scalar overrides
///       c_flags, ld_flags, src, include: [string] (preface with replace to replace the base)
///       dependencies: same shape as the top-level dependencies
///     }
///   }
///
///   dependencies {
///     name: string {
///       (remote | local): string
///       remote: (codeberg | github | bitbucket | git): string (host + "owner/repo")
///       local: string (filesystem path to the dependency)
///       transport: string (ssh | https)
///       build: string (make | cmake | autotools | meson | ninja | xmake | just | conjure,
///                      or a free-form command; conjure = build with conjure)
///       include: [string] (dirs relative to the dep root, each becomes -I<dep>/<dir>)
///       src: [string] (dir-or-file source roots, for manifestless `build: conjure` deps)
///       pkg_config: [string] (packages resolved via pkg-config for cflags/libs)
///       ref: string (branch, tag, or commit to pin; default tracks the default branch)
///     }
///   }
///   output {
///     bin: string (directory for binaries; default "bin")
///     lib: string (directory for libraries; default "lib")
///     symlink_binaries: bool (symlink the built binary into the project root; default false)
///   }
/// }
/// ```
#[derive(Deserialize, Clone, Debug, Serialize)]
pub struct Project {
  #[serde(deserialize_with = "de_slugify")]
  pub name: String,
  pub language: Language,

  #[serde(rename = "type")]
  pub ty: ProjectType,

  #[serde(default, deserialize_with = "de_linkage")]
  pub link: Linkage,

  #[serde(default)]
  pub version: Option<String>,

  #[serde(default)]
  pub license: Option<String>,

  #[serde(default)]
  pub authors: Option<Vec<String>>,

  #[serde(default)]
  pub description: Option<String>,

  #[serde(default)]
  pub compile: Option<Compile>,

  #[serde(default)]
  pub siblings: Option<HashMap<String, Sibling>>,

  #[serde(default)]
  pub tests: Option<HashMap<String, Test>>,

  #[serde(default)]
  pub profiles: Option<HashMap<String, Profile>>,

  #[serde(default)]
  pub dependencies: Option<HashMap<String, Dependency>>,

  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output: Option<Output>,
}

impl Default for Project {
  fn default() -> Self {
    Project {
      name: String::new(),
      language: Language::C,
      ty: ProjectType::Binary,
      link: Linkage::Dynamic,
      version: None,
      license: None,
      authors: None,
      description: None,
      compile: None,
      siblings: None,
      tests: None,
      profiles: None,
      dependencies: None,
      output: None,
    }
  }
}

impl Project {
  /// Whether this project links statically, i.e. `link static`.
  pub fn want_static(&self) -> bool {
    matches!(self.link, Linkage::Static)
  }

  /// Whether this project produces a library rather than a binary.
  pub fn is_library(&self) -> bool {
    matches!(self.ty, ProjectType::Library)
  }

  /// `name` resolved against this project's own profiles, or `None` when it
  /// doesn't define one (the build falls back to default).
  pub fn profile<'a>(
    &'a self,
    name: &'a str,
  ) -> Option<(&'a str, &'a Profile)> {
    self
      .profiles
      .as_ref()
      .and_then(|m| m.get(name))
      .map(|p| (name, p))
  }

  /// The effective project for a build: `Some(profile)` folds its overrides over
  /// the base; `None` returns a clone unchanged.
  pub fn with_profile(&self, profile: Option<&Profile>) -> Project {
    let Some(p) = profile else {
      return self.clone();
    };
    let mut out = self.clone();
    if let Some(ty) = p.ty {
      out.ty = ty;
    }
    if let Some(link) = p.link {
      out.link = link;
    }
    out.compile = Some(out.compile.clone().unwrap_or_default().with_profile(p));
    if let Some(deps) = &p.dependencies {
      let merged = out.dependencies.get_or_insert_with(Default::default);
      for (k, v) in deps {
        merged.insert(k.clone(), v.clone());
      }
    }

    if let Some(tests) = &p.tests {
      let merged = out.tests.get_or_insert_with(Default::default);
      for (k, v) in tests {
        merged.insert(k.clone(), v.clone());
      }
    }

    out
  }

  /// Parse and validate a manifest.
  pub fn from_str(input: &str) -> Result<Self, Error> {
    let project: Project = kdl::de::from_str::<ProjectKDL>(input)?.project;
    if let Some(deps) = &project.dependencies {
      for (name, dep) in deps {
        dep.validate().map_err(|e| {
          Error::Validate(miette::miette!("dependency `{name}`: {e}"))
        })?;
      }
    }

    Ok(project)
  }

  /// Read and parse a manifest from disk.
  pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
    Self::from_str(&fs::read_to_string(path)?)
  }
}

/// KDL wrapper: the document's single `project` node.
#[derive(Deserialize, Serialize, Debug)]
pub struct ProjectKDL {
  pub project: Project,
}

/// Errors from reading, parsing, or validating a project file.
#[derive(Debug, miette::Diagnostic)]
pub enum Error {
  #[diagnostic_source]
  Io(std::io::Error),
  #[diagnostic(transparent)]
  Parse(kdl::de::Error),
  #[diagnostic(transparent)]
  Validate(miette::Report),
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Io(e) => write!(f, "Failed to read project file: {e}"),
      Self::Parse(e) => write!(f, "Failed to parse project file: {e}"),
      Self::Validate(e) => write!(f, "Invalid project file: {e}"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Io(e) => Some(e),
      Self::Parse(e) => Some(e),
      Self::Validate(_) => None,
    }
  }
}

impl From<std::io::Error> for Error {
  fn from(e: std::io::Error) -> Self {
    Self::Io(e)
  }
}

impl From<kdl::de::Error> for Error {
  fn from(e: kdl::de::Error) -> Self {
    Self::Parse(e)
  }
}

impl From<miette::Report> for Error {
  fn from(e: miette::Report) -> Self {
    Self::Validate(e)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::str;

  fn parse(src: &str) -> Project {
    Project::from_str(src).expect("should parse")
  }

  #[test]
  fn slugify_collapses_separators() {
    assert_eq!(slugify("My Cool_Project!!"), "my-cool-project");
    assert_eq!(slugify("--a--b--"), "a-b");
    assert_eq!(slugify(""), "");
  }

  #[test]
  fn language_aliases() {
    assert!(matches!(
      Language::try_from("C".to_string()),
      Ok(Language::C)
    ));
    assert!(matches!(
      Language::try_from("c++".to_string()),
      Ok(Language::Cpp)
    ));
    assert!(matches!(
      Language::try_from("CPP".to_string()),
      Ok(Language::Cpp)
    ));
    assert!(Language::try_from("rust".to_string()).is_err());
  }

  #[test]
  fn project_type_and_linkage_parse() {
    assert_eq!(
      ProjectType::try_from("binary".to_string()).unwrap(),
      ProjectType::Binary
    );
    assert_eq!(
      ProjectType::try_from("Library".to_string()).unwrap(),
      ProjectType::Library
    );
    assert!(ProjectType::try_from("dynamic".to_string()).is_err());

    assert_eq!(
      Linkage::try_from("static".to_string()).unwrap(),
      Linkage::Static
    );
    assert_eq!(
      Linkage::try_from("Dynamic".to_string()).unwrap(),
      Linkage::Dynamic
    );
    assert!(Linkage::try_from("both".to_string()).is_err());
  }

  #[test]
  fn missing_link_defaults_to_dynamic() {
    let p = parse("project {\n  name t\n  language c\n  type binary\n}\n");
    assert_eq!(p.link, Linkage::Dynamic);
    assert!(!p.want_static());
  }

  #[test]
  fn old_two_token_type_errors() {
    let err = Project::from_str(
      "project {\n  name t\n  language c\n  type binary dynamic\n}\n",
    );
    assert!(err.is_err());
  }

  #[test]
  fn flags_list_filters_empty_and_parses_prefix() {
    assert_eq!(
      Flags::try_from(vec!["replace".into(), "-O2".into(), "".into()]).unwrap(),
      Flags::Replace(vec!["-O2".into(), "".into()])
    );
    assert_eq!(
      Flags::Append(vec!["-g".into(), "".into()]).list(),
      vec!["-g".to_string()]
    );
    assert_eq!(Flags::Append(vec![]).list(), Vec::<String>::new());
  }

  #[test]
  fn src_roots_default_and_custom() {
    assert_eq!(Compile::default().src_roots(), vec!["src".to_string()]);
    let c = Compile {
      src: Some(Flags::Append(vec!["lib".into(), "extra".into()])),
      ..Default::default()
    };
    assert_eq!(c.src_roots(), vec!["lib".to_string(), "extra".to_string()]);
  }

  #[test]
  fn with_profile_merges_and_overrides() {
    let project = parse(
      "project {\n  name t\n  language c\n  type library\n  compile {\n    standard c11\n    c_flags -Wall\n    src \"src\"\n  }\n  profiles {\n    test {\n      type binary\n      link static\n      c_flags replace \"-O2\"\n      src append \"extra\"\n    }\n  }\n}\n",
    );
    let p = project.profiles.as_ref().unwrap().get("test").unwrap();
    let eff = project.with_profile(Some(p));

    assert_eq!(eff.ty, ProjectType::Binary); // flipped
    assert!(eff.want_static()); // link overridden
    let compile = eff.compile.as_ref().unwrap();
    // replace wins
    assert_eq!(compile.c_flags, Some(Flags::Replace(vec!["-O2".into()])));
    // append merges onto base
    assert_eq!(
      compile.src_roots(),
      vec!["src".to_string(), "extra".to_string()]
    );
    // untouched base survives
    assert_eq!(compile.standard.as_deref(), Some("c11"));
  }

  #[test]
  fn with_profile_none_is_identity() {
    let project =
      parse("project {\n  name t\n  language c\n  type library\n}\n");
    let eff = project.with_profile(None);
    assert_eq!(eff.ty, ProjectType::Library);
    assert_eq!(eff.name, "t");
  }

  #[test]
  fn project_flags_and_dependencies_merge() {
    let project = parse(
      "project {\n  name t\n  language c\n  type binary\n  dependencies {\n    a {\n      local \"../a\"\n    }\n  }\n  profiles {\n    x {\n      dependencies {\n        b {\n          local \"../b\"\n        }\n      }\n    }\n  }\n}\n",
    );
    let p = project.profiles.as_ref().unwrap().get("x").unwrap();
    let eff = project.with_profile(Some(p));
    let deps = eff.dependencies.as_ref().unwrap();
    assert!(deps.contains_key("a"));
    assert!(deps.contains_key("b"));
  }

  #[test]
  fn dependency_validate_rules() {
    let remote_only = Dependency {
      remote: Some(Remote::GitHub("o/r".into())),
      ..Default::default()
    };
    assert!(remote_only.validate().is_ok());

    let both = Dependency {
      remote: Some(Remote::GitHub("o/r".into())),
      local: Some("../x".into()),
      ..Default::default()
    };
    assert!(both.validate().is_err());

    let neither = Dependency::default();
    assert!(neither.validate().is_err());

    let local_transport = Dependency {
      local: Some("../x".into()),
      transport: Some(Transport::Ssh),
      ..Default::default()
    };
    assert!(local_transport.validate().is_err());
  }

  #[test]
  fn remote_transport_buildsystem_parse() {
    assert_eq!(
      Remote::try_from(vec!["github".into(), "o/r".into()]).unwrap(),
      Remote::GitHub("o/r".into())
    );
    assert!(Remote::try_from(vec!["github".into()]).is_err());
    assert!(Remote::try_from(vec!["gitlab".into(), "o/r".into()]).is_err());

    assert_eq!(
      Transport::try_from("HTTPS".to_string()).unwrap(),
      Transport::Https
    );
    assert!(Transport::try_from("ftp".to_string()).is_err());

    assert_eq!(
      BuildSystem::try_from(vec!["cmake".into(), "libx".into()]).unwrap(),
      BuildSystem::CMake(Some("libx".into()))
    );
    assert_eq!(
      BuildSystem::try_from(vec!["conjure".into()]).unwrap(),
      BuildSystem::Conjure(None)
    );
    assert_eq!(
      BuildSystem::try_from(vec!["just".into()]).unwrap(),
      BuildSystem::Just("build".into())
    );
    assert_eq!(
      BuildSystem::try_from(vec!["sh".into(), "-c".into(), "make".into()])
        .unwrap(),
      BuildSystem::Custom("sh -c make".into())
    );
  }

  #[test]
  fn arch_aliases() {
    assert_eq!(Arch::try_from("x86".to_string()).unwrap(), Arch::X86);
    assert_eq!(Arch::try_from("i686".to_string()).unwrap(), Arch::X86);
    assert_eq!(Arch::try_from("amd64".to_string()).unwrap(), Arch::X86_64);
    assert_eq!(Arch::try_from("AARCH64".to_string()).unwrap(), Arch::Arm64);
    assert!(Arch::try_from("mips".to_string()).is_err());
  }

  #[test]
  fn profile_arch_override() {
    let p = parse(
      "project {\n  name t\n  language c\n  type binary\n  compile {\n    arch x64\n  }\n  profiles {\n    x86 {\n      arch x86\n    }\n  }\n}\n",
    );
    let prof = p.profiles.as_ref().unwrap().get("x86").unwrap();
    assert_eq!(
      p.with_profile(Some(prof)).compile.unwrap().arch,
      Some(Arch::X86)
    );
  }
}
