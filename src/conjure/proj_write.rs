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

fn with_target<S: serde::Serializer>(
  system: &str,
  target: &Option<String>,
  s: S,
) -> Result<S::Ok, S::Error> {
  let mut out = vec![system.to_string()];
  if let Some(t) = target {
    out.push(t.clone());
  }

  out.serialize(s)
}

impl Serialize for BuildSystem {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Custom(str) => str.serialize(s),
      Self::Conjure(t) => with_target("conjure", t, s),
      Self::Make(t) => with_target("make", t, s),
      Self::CMake(t) => with_target("cmake", t, s),
      Self::Autotools(t) => with_target("autotools", t, s),
      Self::Meson(t) => with_target("meson", t, s),
      Self::Ninja(t) => with_target("ninja", t, s),
      Self::Xmake(t) => with_target("xmake", t, s),
      Self::Just(t) => with_target("just", &Some(t.clone()), s),
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
        "compiler & linker settings. `cc` defaults to `cc`; `linker` defaults to\nthe compiler driver, so a bare `cc: clang++` links with clang++ too.\n  standard c11                 // c11 | gnu11 | c17 | c++17 | gnu++20 | ...\n  cc \"ccache gcc\"              // compiler, may carry prefix args\n  linker \"gcc -fuse-ld=mold\"   // linker; gcc/clang/tcc/zig all work\n  src \"src\" \"lib/x\"            // source roots (dirs or files); default [\"src\"]\n  include \"include\" \"third_party\"\n  c_flags -Wall -Wextra        // `replace` prefix overrides base flags\n  ld_flags -flto               // same rules as c_flags\n  threads 8                    // compile/dep parallelism; default = cpu count",
      ),
      (
        "profiles",
        "build profiles: `conjure as <name> -- <subcommand>`, e.g. `conjure as release -- build`\n  profile_name {\n    c_flags replace \"-g\" \"-O0\" // `replace` overrides base, `append` (default) adds\n    ld_flags -flto\n  }",
      ),
      (
        "siblings",
        "co-built sibling conjure projects; each name maps to a directory holding\nits own conjure.kdl, built in the same `conjure build` invocation\n  sibling_name \"libs/sibling_name\"",
      ),
      (
        "dependencies",
        "external dependencies; manage with `conjure add` / `conjure rm`, pin with\n`conjure lock`/`update`\n  dep_name {\n    remote github \"owner/repo\"  // codeberg | github | bitbucket | git\n    transport ssh               // ssh (default) | https\n    local \"../path/to/dep\"      // instead of remote\n    build cmake libname         // make | cmake | meson | ninja | xmake | autotools | just | conjure, or a raw command\n    include \"include\"\n    pkg_config \"pkg\"\n    ref \"v1.0.0\"\n  }",
      ),
    ] {
      if let Some(n) = children.get_mut(name) {
        n.ensure_children();
        let mut fmt = n.format().cloned().unwrap_or_default();
        let mut lines = vec![String::new()]; // leading `//` spacer
        lines.extend(comment.lines().map(|l| format!("// {l}")));
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
          lines.pop();
        }
        fmt.leading = lines.join("\n") + "\n";
        n.set_format(fmt);
      }
    }
  }

  let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
  doc.autoformat_config(&cfg);

  fs::write(path, fix_braces(&doc.to_string()))?;
  Ok(())
}
