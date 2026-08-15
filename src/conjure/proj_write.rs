use super::proj_parse::{
  BuildSystem, Flags, Language, Project, ProjectKDL, ProjectType, Remote, Transport,
};
use miette::Result;
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Debug, miette::Diagnostic)]
pub enum Error {
  #[diagnostic_source]
  Io(std::io::Error),
  Se(kdl::se::Error),
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Io(e) => write!(f, "failed to write project file: {e}"),
      Self::Se(e) => write!(f, "failed to serialize project file: {e}"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Io(e) => Some(e),
      Self::Se(e) => Some(e),
    }
  }
}

impl From<std::io::Error> for Error {
  fn from(e: std::io::Error) -> Self {
    Self::Io(e)
  }
}

impl From<kdl::se::Error> for Error {
  fn from(e: kdl::se::Error) -> Self {
    Self::Se(e)
  }
}

impl Serialize for ProjectType {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    let (kind, linkage) = match self {
      Self::BinaryDynamic => ("binary", "dynamic"),
      Self::BinaryStatic => ("binary", "static"),
      Self::LibraryDynamic => ("library", "dynamic"),
      Self::LibraryStatic => ("library", "static"),
    };
    (kind, linkage).serialize(s)
  }
}

impl Serialize for Flags {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Append(v) => v.serialize(s),
      Self::Replace(v) => {
        let mut out = vec!["replace".to_string()];
        out.extend(v.iter().cloned());
        out.serialize(s)
      }
    }
  }
}

impl Serialize for Remote {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    let (host, path) = match self {
      Self::Codeberg(path) => ("codeberg", path),
      Self::GitHub(path) => ("github", path),
      Self::BitBucket(path) => ("bitbucket", path),
      Self::Git(path) => ("git", path),
    };

    (host, path).serialize(s)
  }
}

impl Serialize for Transport {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Ssh => "ssh".serialize(s),
      Self::Https => "https".serialize(s),
    }
  }
}

impl Serialize for BuildSystem {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Custom(str) => str.serialize(s),
      Self::Make => "make".serialize(s),
      Self::CMake => "cmake".serialize(s),
      Self::Autotools => "autotools".serialize(s),
      Self::Meson => "meson".serialize(s),
      Self::Ninja => "ninja".serialize(s),
      Self::Xmake => "xmake".serialize(s),
    }
  }
}

impl Serialize for Language {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::C => "c".serialize(s),
      Self::Cpp => "c++".serialize(s),
    }
  }
}

pub fn fix_braces(s: &str) -> String {
  let lines: Vec<&str> = s.lines().collect();
  let mut out = String::new();
  let mut stack: Vec<String> = vec![];

  for (i, line) in lines.iter().enumerate() {
    if line.trim() == "}" {
      let indent = stack.pop().unwrap_or_default();
      out.push_str(&indent);
      out.push_str("}\n");
      if stack.len() == 1 {
        let next = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
        if next.is_none_or(|l| l.trim() != "}") {
          out.push('\n');
        }
      }
    } else {
      if line.trim_end().ends_with('{') {
        let lead = &line[..line.len() - line.trim_start().len()];
        stack.push(lead.to_string());
      }
      out.push_str(line);
      out.push('\n');
    }
  }
  out
}

pub fn write(path: impl AsRef<Path>, project: Project) -> Result<(), Error> {
  let mut doc = kdl::se::to_document(&ProjectKDL { project })?;
  if let Some(children) = doc
    .get_mut("project")
    .and_then(|n| n.children_mut().as_mut())
  {
    for (name, comment) in [
      (
        "compile",
        "compiler & linker settings; add `cc`, `c_flags`, `ld_flags`, `standard`",
      ),
      (
        "sub_projects",
        "nested sub-projects; bare path (subproject has its own conjure.kdl) or map with type/compile/dependencies for non-conjure sub-projects",
      ),
      (
        "profiles",
        "build profiles, e.g. debug { c_flags replace \"-g\" \"-O0\"}",
      ),
      (
        "dependencies",
        "dependencies; map with remote/transport/build for remote dependencies; add with conjure add",
      ),
    ] {
      if let Some(n) = children.get_mut(name) {
        n.ensure_children();
        let mut fmt = n.format().cloned().unwrap_or_default();
        fmt.leading = format!("// {comment}\n");
        n.set_format(fmt);
      }
    }
  }

  let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
  doc.autoformat_config(&cfg);

  fs::write(path, fix_braces(&doc.to_string()))?;
  Ok(())
}
