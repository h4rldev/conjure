//! Project scaffolding for `conjure new` / `conjure init`.
//!
//! Writes a minimal but buildable C or C++ project: `src/` and `include/`
//! directories, a hello-world source, and the `conjure.kdl` manifest. The C++
//! source is chosen by standard so a C++23 project gets the `import std;` form
//! instead of a preprocessor hello.

/***********************************************************************/

use super::{
  proj_parse::{Language, Project},
  proj_write,
};
use miette::{IntoDiagnostic, Result};
use std::{fs, path::Path};

/***********************************************************************/

const CPP_HELLO: &str = r#"#include <iostream>

int main() {
  std::cout << "Hello, world!\n";
  return 0;
}
"#;

const CPP23_HELLO: &str = r#"import std;

int main() {
  std::println("Hello, world!");
  return 0;
}
"#;

const C_HELLO: &str = r#"#include <stdio.h>

int main(void) {
  puts("Hello, world!");
  return 0;
}
"#;

/// The major version of a `c++`/`gnu++` standard string, e.g. `c++23` -> `23`.
fn std_major(s: &str) -> Option<u32> {
  s.strip_prefix("gnu++")
    .or_else(|| s.strip_prefix("c++"))?
    .parse()
    .ok()
}

/// Create a project tree at `path` from `project`.
///
/// The manifest is written last, so a failure partway through leaves a dir with
/// no `conjure.kdl` rather than a manifest pointing at a half-made tree.
pub fn make_proj(path: impl AsRef<Path>, project: Project) -> Result<()> {
  let proj_path = path.as_ref();

  fs::create_dir_all(proj_path.join("src")).into_diagnostic()?;
  fs::create_dir_all(proj_path.join("include")).into_diagnostic()?;

  if let Language::Cpp = &project.language {
    // C++23 gets `import std;`; anything else (including no standard) gets the
    // preprocessor hello.
    let main_cpp = project
      .compile
      .as_ref()
      .and_then(|c| c.standard.as_deref())
      .and_then(std_major)
      .filter(|major| *major >= 23)
      .map_or(CPP_HELLO, |_| CPP23_HELLO);

    fs::write(proj_path.join("src/main.cpp"), main_cpp).into_diagnostic()?;
  } else {
    fs::write(proj_path.join("src/main.c"), C_HELLO).into_diagnostic()?;
  }

  proj_write::write(proj_path.join("conjure.kdl"), project)?;

  Ok(())
}
