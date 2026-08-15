use super::{
  proj_parse::{Language, Project},
  proj_write,
};
use miette::{IntoDiagnostic, Result};
use std::{fs, path::Path};

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

fn std_major(s: &str) -> Option<u32> {
  s.strip_prefix("gnu++")
    .or_else(|| s.strip_prefix("c++"))?
    .parse()
    .ok()
}

pub fn make_proj(path: impl AsRef<Path>, project: Project) -> Result<()> {
  let proj_path = path.as_ref();
  let proj = project.clone();
  let standard = proj.compile.unwrap().standard.unwrap();

  fs::create_dir_all(proj_path.join("src")).into_diagnostic()?;
  fs::create_dir_all(proj_path.join("include")).into_diagnostic()?;

  if let Language::Cpp = &project.language {
    let main_cpp = match std_major(standard.as_ref()) {
      Some(major) if major >= 23 => CPP23_HELLO,
      _ => CPP_HELLO,
    };

    fs::write(proj_path.join("src/main.cpp"), main_cpp).into_diagnostic()?;
  } else {
    fs::write(proj_path.join("src/main.c"), C_HELLO).into_diagnostic()?;
  }

  proj_write::write(proj_path.join("conjure.kdl"), project)?;

  Ok(())
}
