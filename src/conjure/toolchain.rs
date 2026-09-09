use super::proj_parse::Compile;

/// Compiler CLI dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerStyle {
  /// gcc/clang/zig cc/tcc and every cross compiler (`arm-none-eabi-gcc`):
  /// `-std=`, `-I`, `-c src -o obj`, `-shared`, `-static`, `-fPIC`.
  Gnu,
  /// cl.exe / clang-cl / icx: `/std:`, `/I`, `/c src /Fo:obj`, `/DLL`.
  Msvc,
}

/// How the configured linker program behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStyle {
  /// A compiler driver that performs the link (cc/gcc/clang): knows crt files,
  /// resolves `-l`, accepts `-shared`/`-static`.
  Driver,
  /// A raw linker: ld, ld.lld, mold, gold, link.exe. conjure passes objects,
  /// full lib paths and flags; no driver-isms.
  Raw,
}

/// Resolved once per build; all argv generation goes through this so
/// `compile.rs`/`link.rs` never hardcode a dialect again.
#[derive(Debug, Clone)]
pub struct Toolchain {
  pub cc: Vec<String>,
  pub style: CompilerStyle,
  pub linker: Vec<String>,
  pub _link: LinkStyle,
}

impl Toolchain {
  pub fn obj_ext(&self) -> &'static str {
    match self.style {
      CompilerStyle::Gnu => "o",
      CompilerStyle::Msvc => "obj",
    }
  }

  /// conjure-owned semantics translated into the compiler's dialect.
  pub fn std_args(&self, standard: &str) -> Vec<String> {
    match self.style {
      CompilerStyle::Gnu => vec![format!("-std={standard}")],
      CompilerStyle::Msvc => vec![format!("/std:{standard}")],
    }
  }

  pub fn include_arg(&self, dir: &str) -> String {
    match self.style {
      CompilerStyle::Gnu => format!("-I{dir}"),
      CompilerStyle::Msvc => format!("/I{dir}"),
    }
  }

  /// Objects feeding a shared library must be PIC; COFF is PIC by default.
  pub fn pic_flag(&self) -> Option<String> {
    match self.style {
      CompilerStyle::Gnu => Some("-fPIC".into()),
      CompilerStyle::Msvc => None,
    }
  }

  /// The compile-only + output tail.
  pub fn compile_tail(&self, src: &str, obj: &str) -> Vec<String> {
    match self.style {
      CompilerStyle::Gnu => vec!["-c".into(), src.into(), "-o".into(), obj.into()],
      CompilerStyle::Msvc => vec!["/c".into(), src.into(), format!("/Fo:{obj}")],
    }
  }

  /// Whole-program static link, dialect permitting.
  // ponytail: msvc has no `-static`; real msvc static = /MT + static dep paths, add with B.
  pub fn static_flag(&self) -> Option<String> {
    match self.style {
      CompilerStyle::Gnu => Some("-static".into()),
      CompilerStyle::Msvc => None,
    }
  }
}

fn split(tool: &str) -> Vec<String> {
  tool.split_whitespace().map(|s| s.to_string()).collect()
}

/// Basename of the actual tool, unwrapping transparent wrappers once.
fn real_tool(argv: &[String]) -> &str {
  let mut name = argv.first().map(String::as_str).unwrap_or("cc");
  if matches!(name, "ccache" | "sccache" | "distcc" | "icecc") {
    name = argv.get(1).map(String::as_str).unwrap_or("cc");
  }
  name.rsplit(['/', '\\']).next().unwrap_or(name)
}

// Extending conjure to a new tool = one row here.
fn compiler_style(argv: &[String]) -> CompilerStyle {
  match real_tool(argv) {
    "cl" | "clang-cl" | "icx" | "icl" => CompilerStyle::Msvc,
    _ => CompilerStyle::Gnu, // every unknown cross compiler speaks gnu
  }
}

fn linker_style(argv: &[String]) -> LinkStyle {
  match real_tool(argv) {
    "ld" | "ld.bfd" | "ld.gold" | "ld.lld" | "lld" | "lld-link" | "mold" | "gold" | "link" => {
      LinkStyle::Raw
    }
    _ => LinkStyle::Driver,
  }
}

pub fn resolve(compile: &Compile) -> Toolchain {
  let cc = compile
    .cc
    .as_deref()
    .filter(|s| !s.trim().is_empty())
    .map(split)
    .unwrap_or_else(|| vec!["cc".into()]);

  let (linker, link) = match compile.linker.as_deref().filter(|s| !s.trim().is_empty()) {
    Some(l) => {
      let l = split(l);
      let style = linker_style(&l);
      (l, style)
    }
    // default linker = the compiler driver (fixes link.rs defaulting to a
    // literal "cc" even when the user set cc: clang++)
    None => (cc.clone(), LinkStyle::Driver),
  };

  Toolchain {
    style: compiler_style(&cc),
    cc,
    linker,
    _link: link,
  }
}
