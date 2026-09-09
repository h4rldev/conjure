use miette::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path, string::String};

/// Slugify a string for deserialization
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

/// Implement a custom deserializer to slugify strings in a project name
fn de_slugify<'de, D>(d: D) -> Result<String, D::Error>
where
  D: serde::Deserializer<'de>,
{
  Ok(slugify(&String::deserialize(d)?))
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ProjectKDL {
  pub project: Project,
}

/// The project file format
///
/// # Schema
///
/// ```
/// project {
///   name: string (slugifies in deserialization),
///   language: string (matches any known form of C or C++ such as C, C++, Cpp, etc.)
///   src: string (path to the source directory)
///   license: string
///   authors: [string]
///   description: string
///   type: [string] (matches any known type of project such as binary, library, followed by link
///   convention, such as dynamic or static)
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
///   }
///
///   siblings {
///     name path: string (a sibling conjure project with its own conjure.kdl, built
///     alongside this one in the same `conjure build`)
///   }
///
///   profiles {
///     name: string {
///       c_flags: [string] (preface with replace to replace the base flags; default appends)
///       ld_flags: [string] (same as c_flags)
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
///     symlink_binaries: bool (symlink the built binary into the project root for
///                            convenience when you don't want to use `conjure run`;
///                            default false)
///   }
/// }
/// ```
///
/// The artifact layout under `bin`/`lib` is:
///   bin/<project>/<profile>/<binary>      e.g. bin/cheese/debug/cheese
///   lib/<project>/<profile>/lib<project>.{a|so}
/// where <profile> is the active profile name (default "default"), and <project>
/// scopes each project's artifacts so co-built siblings don't collide.
///
#[derive(Deserialize, Clone, Debug, Serialize)]
pub struct Project {
  #[serde(deserialize_with = "de_slugify")]
  pub name: String,
  pub language: Language,
  #[serde(rename = "type")]
  pub ty: ProjectType,

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
      ty: ProjectType::BinaryDynamic,
      version: None,
      license: None,
      authors: None,
      description: None,
      compile: None,
      siblings: None,
      profiles: None,
      dependencies: None,
      output: None,
    }
  }
}

#[derive(Deserialize, Default, Clone, Debug)]
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
#[serde(try_from = "Vec<String>")]
pub enum ProjectType {
  #[default]
  BinaryDynamic,
  BinaryStatic,
  LibraryDynamic,
  LibraryStatic,
}

impl TryFrom<Vec<String>> for ProjectType {
  type Error = String;
  fn try_from(v: Vec<String>) -> Result<Self, Self::Error> {
    let a = v.first().map(|s| s.to_ascii_lowercase());
    let b = v.get(1).map(|s| s.to_ascii_lowercase());

    match (a.as_deref(), b.as_deref()) {
      (Some("binary"), Some("dynamic")) => Ok(Self::BinaryDynamic),
      (Some("binary"), Some("static")) => Ok(Self::BinaryStatic),
      (Some("library"), Some("dynamic")) => Ok(Self::LibraryDynamic),
      (Some("library"), Some("static")) => Ok(Self::LibraryStatic),
      other => Err(format!("unknown project type: {other:?}")),
    }
  }
}

#[derive(Deserialize, Default, Serialize, Clone, Debug)]
pub struct Compile {
  pub cc: Option<String>,
  pub linker: Option<String>,
  pub standard: Option<String>,
  pub include: Option<Vec<String>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub src: Option<Vec<String>>,
  #[serde(default)]
  pub c_flags: Option<Flags>,
  #[serde(default)]
  pub ld_flags: Option<Flags>,
  #[serde(default)]
  pub threads: Option<usize>,
}

impl Compile {
  pub fn src_roots(&self) -> Vec<String> {
    self.src.clone().unwrap_or(vec!["src".to_string()])
  }
}

#[derive(Deserialize, Clone, Serialize, Debug)]
pub struct Sibling {
  #[serde(rename = "#0")]
  pub path: String,
}

#[derive(Deserialize, Clone, Serialize, Debug)]
pub struct Profile {
  #[serde(default)]
  pub c_flags: Option<Flags>,
  #[serde(default)]
  pub ld_flags: Option<Flags>,
}

#[derive(Deserialize, Default, Clone, Serialize, Debug)]
pub struct Output {
  pub bin: Option<String>,
  pub lib: Option<String>,
  pub symlink_binaries: Option<bool>,
}

#[derive(Deserialize, Clone, Serialize, Debug)]
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
  pub fn validate(&self) -> Result<(), String> {
    match (&self.remote, &self.local) {
      (Some(_), Some(_)) => Err("remote and local cannot be used at the same time".into()),
      (None, None) => Err("remote or local must be used".into()),
      (None, Some(_)) if self.transport.is_some() => {
        Err("transport cannot be used with local".into())
      }
      _ => Ok(()),
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
      Self::Io(e) => write!(f, "failed to read project file: {e}"),
      Self::Parse(e) => write!(f, "failed to parse project file: {e}"),
      Self::Validate(e) => write!(f, "invalid project file: {e}"),
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

impl Project {
  pub fn from_str(input: &str) -> Result<Self, Error> {
    let project: Project = kdl::de::from_str::<ProjectKDL>(input)?.project;
    if let Some(deps) = &project.dependencies {
      for (name, dep) in deps {
        dep
          .validate()
          .map_err(|e| Error::Validate(miette::miette!("dependency `{name}`: {e}")))?;
      }
    }

    Ok(project)
  }

  pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
    Self::from_str(&fs::read_to_string(path)?)
  }
}
