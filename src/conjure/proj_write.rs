//! Writing `conjure.kdl`: serialization of a [`Project`] plus the KDL formatting
//! fixups this project needs.
//!
//! `kdl`'s serializer drops value quoting and can mis-place the `project` node's
//! braces, so generated manifests are passed through [`fix_braces`] (balanced
//! brace indentation, blank lines between top-level sections). The `Serialize`
//! impls here exist because the covered enums must render as bare KDL tokens
//! (`type binary`, `link static`, `remote github "o/r"`) rather than serde's
//! default shapes; they mirror the declaration order in `proj_parse`.
//!
//! Every `conjure new`/`init` manifest is written through here, and `conjure
//! add`/`rm` round-trip their edits through it.

/***********************************************************************/

use super::proj_parse::{
  Arch, BuildSystem, Flags, Language, Linkage, Project, ProjectKDL,
  ProjectType, Remote, Transport,
};
use miette::Result;
use serde::Serialize;
use std::{fs, path::Path};

/***********************************************************************/

const SECTION_COMMENTS: &[(&str, &str)] = &[
  (
    "compile",
    "compiler & linker settings. `cc` defaults to `cc`; `linker` defaults to\nthe compiler driver, so a bare `cc: clang++` links with clang++ too.\n  standard c11                 // c11 | gnu11 | c17 | c++17 | gnu++20 | ...\n  cc \"ccache gcc\"              // compiler, may carry prefix args\n  linker \"gcc -fuse-ld=mold\"   // linker; gcc/clang/tcc/zig all work\n  src \"src\" \"lib/x\"            // source roots (dirs or files); default [\"src\"]\n  include \"include\" \"third_party\"\n  c_flags -Wall -Wextra        // `replace` prefix overrides base flags\n  ld_flags -flto               // same rules as c_flags\n  threads 8                    // compile/dep parallelism; default = cpu count",
  ),
  (
    "profiles",
    "build profiles: `conjure as <name> -- <subcommand>`, e.g. `conjure as release -- build`\n  profile_name {\n    c_flags replace \"-g\" \"-O0\" // `replace` overrides base, `append` (default) adds\n    ld_flags -flto\n    tests { }                // profile-specific tests, merged over the top-level set\n  }",
  ),
  (
    "siblings",
    "co-built sibling conjure projects; each name maps to a directory holding\nits own conjure.kdl, built in the same `conjure build` invocation\n  sibling_name \"libs/sibling_name\"",
  ),
  (
    "tests",
    "test targets: each name compiles to a binary that links the project's\nlibrary, so the project must be `type library`; built by `conjure test` and\nnever run by conjure\n  test_name { src \"tests/test_name.c\" }",
  ),
  (
    "dependencies",
    "external dependencies; manage with `conjure add` / `conjure rm`, pin with\n`conjure lock`/`update`\n  dep_name {\n    remote github \"owner/repo\"  // codeberg | github | bitbucket | git\n    transport ssh               // ssh (default) | https\n    local \"../path/to/dep\"      // instead of remote\n    build cmake libname         // make | cmake | meson | ninja | xmake | autotools | just | conjure, or a raw command\n    include \"include\"\n    pkg_config \"pkg\"\n    ref \"v1.0.0\"\n  }",
  ),
];

/// Errors from serializing or writing a project file.
#[derive(Debug, miette::Diagnostic)]
pub enum Error {
  #[diagnostic_source]
  Io(std::io::Error),
  Se(kdl::se::Error),
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Io(e) => write!(f, "Failed to write project file: {e}"),
      Self::Se(e) => write!(f, "Failed to serialize project file: {e}"),
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

impl Serialize for Language {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::C => "c".serialize(s),
      Self::Cpp => "c++".serialize(s),
    }
  }
}

impl Serialize for ProjectType {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Binary => "binary".serialize(s),
      Self::Library => "library".serialize(s),
    }
  }
}

impl Serialize for Linkage {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Dynamic => "dynamic".serialize(s),
      Self::Static => "static".serialize(s),
    }
  }
}

impl Serialize for Arch {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    match self {
      Self::Native => "native".serialize(s),
      Self::X86 => "x86".serialize(s),
      Self::X86_64 => "x64".serialize(s),
      Self::Arm64 => "arm64".serialize(s),
    }
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

/// Render a build system as `system [target]` tokens; the only consumer is
/// [`BuildSystem`]'s `Serialize`.
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

/// Re-indent KDL by tracking brace depth, and insert a blank line between
/// top-level sections.
///
/// Comment lines are emitted verbatim and never counted as braces: the template
/// comments contain brace examples (`//   profile_name {`), and treating those
/// as real blocks would desync the stack and mis-indent the file's final `}`.
pub fn fix_braces(s: &str) -> String {
  let lines: Vec<&str> = s.lines().collect();
  let mut out = String::new();
  let mut stack: Vec<String> = vec![];

  for (i, line) in lines.iter().enumerate() {
    if line.trim_start().starts_with("//") {
      out.push_str(line);
      out.push('\n');
      continue;
    }

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

/// Serialize `project` to `path`, annotating its nodes with the example comments
/// from [`SECTION_COMMENTS`].
pub fn write(path: impl AsRef<Path>, project: Project) -> Result<(), Error> {
  let mut doc = kdl::se::to_document(&ProjectKDL { project })?;
  if let Some(children) = doc
    .get_mut("project")
    .and_then(|n| n.children_mut().as_mut())
  {
    for (name, comment) in SECTION_COMMENTS {
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fix_braces_ignores_comment_braces() {
    let input = "project {\n  // example {\n  //   inner {\n  // }\n  compile {\n    a b\n  }\n}\n";
    let out = fix_braces(input);
    assert_eq!(out.lines().last(), Some("}"));
    assert!(out.contains("// example {"));
  }

  #[test]
  fn fix_braces_nests_blocks() {
    let input = "project {\n    compile {\n        x y\n    }\n}\n";
    let out = fix_braces(input);
    assert_eq!(out, "project {\n    compile {\n        x y\n    }\n}\n");
  }

  #[test]
  fn serialize_enum_tokens() {
    let project: Project = kdl::de::from_str::<ProjectKDL>(
      "project {\n  name s\n  language c\n  type library\n  link static\n}\n",
    )
    .unwrap()
    .project;
    let doc = kdl::se::to_document(&ProjectKDL { project })
      .unwrap()
      .to_string();
    assert!(doc.contains("type library"));
    assert!(doc.contains("link static"));
  }
}
