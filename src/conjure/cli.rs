//! Command-line interface: argument parsing and the `handle_*` implementations
//! for every subcommand.
//!
//! The clap types at the top are the sole source of the CLI surface; each
//! subcommand's `handle_*` below does the work. Most subcommands operate on the
//! manifest in the current directory (`.conjure.kdl`), while `conjure add`/`rm`
//! edit it in place through `proj_write`'s helpers and `conjure as` re-enters the
//! CLI under an active profile.

/***********************************************************************/

use super::{
  build, compile, git, lock, proj,
  proj_parse::{
    self, Arch, Compile, Flags, Linkage, Output, Profile, Project, ProjectType,
    slugify,
  },
  proj_write::fix_braces,
  ui::{StepStatus, Ui},
};
use clap::{
  Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
  builder::{Styles, styling::AnsiColor},
};
use kdl::{FormatConfigBuilder, KdlDocument, KdlNode};
use miette::{IntoDiagnostic, Result};
use owo_colors::OwoColorize;
use std::{
  collections::{HashMap, HashSet},
  env, fs,
  path::Path,
};

/***********************************************************************/

const LONG_ABOUT: &str = r#"
Conjure, the modern build-tool for C and C++.
Made by h4rl, for everyone.
"#;

const HELP_TEMPLATE: &str = "{about}\n{usage-heading} {usage}\n\n{all-args}";

#[derive(Clone, ValueEnum, Default)]
enum Language {
  #[value(alias = "c")]
  #[default]
  C,
  #[value(alias = "c++")]
  Cpp,
}

impl std::fmt::Display for Language {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Language::C => write!(f, "C"),
      Language::Cpp => write!(f, "C++"),
    }
  }
}

#[derive(Clone, ValueEnum, Default)]
enum TypeType {
  #[value(alias = "binary")]
  #[default]
  Binary,
  #[value(alias = "library")]
  Library,
}

impl std::fmt::Display for TypeType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TypeType::Binary => write!(f, "binary"),
      TypeType::Library => write!(f, "library"),
    }
  }
}

#[derive(Clone, ValueEnum, Default)]
enum LinkType {
  #[value(alias = "static")]
  Static,
  #[value(alias = "dynamic")]
  #[default]
  Dynamic,
}

impl std::fmt::Display for LinkType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      LinkType::Static => write!(f, "static"),
      LinkType::Dynamic => write!(f, "dynamic"),
    }
  }
}

#[derive(Clone, ValueEnum, Default)]
enum ArchType {
  #[value(alias = "native")]
  #[default]
  Native,
  #[value(alias = "x86")]
  X86,
  #[value(alias = "x64")]
  X64,
  #[value(alias = "arm64")]
  Arm64,
}

impl std::fmt::Display for ArchType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ArchType::Native => write!(f, "native"),
      ArchType::X86 => write!(f, "x86"),
      ArchType::X64 => write!(f, "x64"),
      ArchType::Arm64 => write!(f, "arm64"),
    }
  }
}

#[derive(Clone, ValueEnum, Debug)]
enum Transport {
  #[value(alias = "ssh")]
  Ssh,
  #[value(alias = "https")]
  Https,
}

#[derive(Clone, ValueEnum)]
enum BuildSystem {
  #[value(alias = "make")]
  Make,
  #[value(alias = "cmake")]
  Cmake,
  #[value(alias = "autotools")]
  Autotools,
  #[value(alias = "meson")]
  Meson,
  #[value(alias = "ninja")]
  Ninja,
  #[value(alias = "xmake")]
  Xmake,
  #[value(alias = "just")]
  Just,
}

#[derive(Clone, ValueEnum)]
enum LocalOrRemote {
  #[value(alias = "local")]
  Local,
  #[value(alias = "codeberg")]
  Codeberg,
  #[value(alias = "github")]
  Github,
  #[value(alias = "bitbucket")]
  Bitbucket,
  #[value(alias = "git")]
  Git,
}

#[derive(Args)]
struct NewArgs {
  /// Name of the project
  name: String,
  /// Language of the project
  #[arg(long, short = 'l', value_enum)]
  language: Option<Language>,
  /// Which C/C++ standard to use
  #[arg(long, short = 's')]
  standard: Option<String>,
  /// Compile type of the project
  #[arg(long = "type", value_enum)]
  ty: Option<TypeType>,
  /// Link type of the project
  #[arg(long, value_enum)]
  link: Option<LinkType>,
  /// Force the creation of the project
  #[arg(long)]
  force: bool,
  /// Target architecture
  #[arg(long, short = 'a', value_enum)]
  arch: Option<ArchType>,
}

#[derive(Args)]
struct InitArgs {
  /// Name of the project, if not provided, the current directory's name will be used
  #[arg(long, short = 'n')]
  name: Option<String>,
  /// Language of the project
  #[arg(long, short = 'l', value_enum)]
  language: Option<Language>,
  /// Which C/C++ standard to use
  #[arg(long, short = 's')]
  standard: Option<String>,
  /// Compile type of the project
  #[arg(long = "type", value_enum)]
  ty: Option<TypeType>,
  /// Link type of the project
  #[arg(long, value_enum)]
  link: Option<LinkType>,
  /// Force the creation of the project
  #[arg(long)]
  force: bool,
  /// Target architecture
  #[arg(long, short = 'a', value_enum)]
  arch: Option<ArchType>,
}

#[derive(Args)]
struct AddArgs {
  /// The type of dependency
  local_or_remote: LocalOrRemote,
  /// Path or URL of the dependency
  path_or_url: String,
  /// Build system to use for the dependency
  build: Option<BuildSystem>,
  /// Transport to use for getting the dependency
  #[arg(long, value_enum)]
  transport: Option<Transport>,
  /// Tag, branch, or commit to pin
  #[arg(long, short = 'r')]
  r#ref: Option<String>,
  /// Force the addition of the dependency
  #[arg(long)]
  force: bool,
}

#[derive(Args)]
struct RemoveArgs {
  /// Dependency to remove
  name: String,
}

#[derive(Args)]
struct UpdateArgs {
  /// Dependency to update
  name: Option<String>,
}

#[derive(Args)]
struct BuildArgs {
  /// Build profile to use
  #[arg(long, short = 'p')]
  profile: Option<String>,
  /// Force a build even if nothing has changed
  #[arg(long, short = 'f')]
  force: bool,
  #[arg(long, short = 's')]
  no_siblings: bool,
  #[arg(long, short = 'j', alias = "jobs")]
  threads: Option<usize>,
}

#[derive(Args)]
struct CompileCommandsArgs {
  /// Profile to use for making the compile_commands.json
  #[arg(long, short = 'p')]
  profile: Option<String>,
  /// Don't build siblings
  #[arg(long, short = 's')]
  no_siblings: bool,
}

#[derive(Args)]
struct AsArgs {
  /// Profile to use for the subcommand
  profile: String,
  #[command(subcommand)]
  subcommand: AsCommands,
}

#[derive(Args)]
struct TestArgs {
  /// Names of the tests to build (If empty, build all)
  names: Vec<String>,
  /// Profile to use for building the tests
  #[arg(long, short = 'p')]
  profile: Option<String>,
  /// Threads to use for building the tests
  #[arg(long, short = 'j', alias = "jobs")]
  threads: Option<usize>,
}

/// Subcommands `conjure as` forwards to. Mirrors [`Commands`] minus `As`:
/// nesting `As` would recurse clap's help tree forever.
#[derive(Subcommand)]
enum AsCommands {
  /// Create a new Conjure project
  New(NewArgs),
  /// Initialize a Conjure project in the current directory
  Init(InitArgs),
  /// Add a dependency to the project
  Add(AddArgs),
  /// Remove a dependency from the project
  #[command(alias = "remove")]
  Rm(RemoveArgs),
  /// Lock the project's dependencies
  Lock,
  /// Update the project's dependencies, and recache them.
  Update(UpdateArgs),
  /// Build the project
  Build(BuildArgs),
  /// Generate a compile_commands.json for clangd
  #[command(name = "compile-commands")]
  Compile(CompileCommandsArgs),
  /// Build the project's tests
  Test(TestArgs),
}

#[derive(Subcommand)]
enum Commands {
  /// Create a new Conjure project
  New(NewArgs),
  /// Initialize a Conjure project in the current directory
  Init(InitArgs),
  /// Add a dependency to the project
  Add(AddArgs),
  /// Remove a dependency from the project
  #[command(alias = "remove")]
  Rm(RemoveArgs),
  /// Lock the project's dependencies
  Lock,
  /// Update the project's dependencies, and recache them.
  Update(UpdateArgs),
  /// Build the project
  Build(BuildArgs),
  /// Generate a compile_commands.json for clangd
  #[command(name = "compile-commands")]
  Compile(CompileCommandsArgs),
  /// Run a subcommand as a specific profile
  As(AsArgs),
  /// Build the project's tests
  Test(TestArgs),
}

impl From<AsCommands> for Commands {
  fn from(cmd: AsCommands) -> Self {
    match cmd {
      AsCommands::New(args) => Commands::New(args),
      AsCommands::Init(args) => Commands::Init(args),
      AsCommands::Add(args) => Commands::Add(args),
      AsCommands::Rm(args) => Commands::Rm(args),
      AsCommands::Lock => Commands::Lock,
      AsCommands::Update(args) => Commands::Update(args),
      AsCommands::Build(args) => Commands::Build(args),
      AsCommands::Compile(args) => Commands::Compile(args),
      AsCommands::Test(args) => Commands::Test(args),
    }
  }
}

fn styles() -> Styles {
  Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::BrightBlue.on_default())
    .placeholder(AnsiColor::BrightCyan.on_default())
}

#[derive(Parser)]
#[command(version, about, long_about = Some(LONG_ABOUT), help_template = HELP_TEMPLATE)]
#[command(propagate_version = true)]
#[command(styles = styles())]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

fn project_arch(arch: ArchType) -> Arch {
  match arch {
    ArchType::X86 => Arch::X86,
    ArchType::X64 => Arch::X86_64,
    ArchType::Arm64 => Arch::Arm64,
    ArchType::Native => Arch::Native,
  }
}

fn project_type(ty: Option<TypeType>) -> ProjectType {
  match ty {
    Some(TypeType::Binary) => ProjectType::Binary,
    Some(TypeType::Library) => ProjectType::Library,
    None => ProjectType::default(),
  }
}

fn project_link(link: Option<LinkType>) -> Linkage {
  match link {
    Some(LinkType::Static) => Linkage::Static,
    Some(LinkType::Dynamic) => Linkage::Dynamic,
    None => Linkage::default(),
  }
}

fn default_standard(language: Option<Language>) -> &'static str {
  match language {
    Some(Language::Cpp) => "c++17",
    _ => "c11",
  }
}

/// The one-line success message shared by `new` and `init`, showing the values
/// the project was actually created with (defaults filled in).
fn created_summary(
  name: &str,
  language: Option<Language>,
  standard: Option<String>,
  ty: Option<TypeType>,
  link: Option<LinkType>,
  arch: Option<ArchType>,
) -> String {
  format!(
    "Created project {} with language {}, standard {}, type {}, link {}, and arch {}",
    name.bold().green(),
    language.clone().unwrap_or_default().bold().blue(),
    standard
      .unwrap_or_else(|| default_standard(language).to_string())
      .bold()
      .yellow(),
    ty.unwrap_or_default().bold().purple(),
    link.unwrap_or_default().bold().purple(),
    arch.unwrap_or_default().bold().cyan()
  )
}

/// Assemble the manifest for a freshly created project, including the default
/// `debug`/`release` profiles.
fn build_project(
  name: String,
  language: Option<Language>,
  standard: Option<String>,
  ty: Option<TypeType>,
  link: Option<LinkType>,
  arch: Option<ArchType>,
) -> Project {
  Project {
    name,
    language: match language {
      Some(Language::Cpp) => crate::conjure::proj_parse::Language::Cpp,
      _ => crate::conjure::proj_parse::Language::C,
    },
    compile: Some(Compile {
      standard: Some(
        standard.unwrap_or_else(|| default_standard(language).to_string()),
      ),
      arch: arch.map(project_arch),
      ..Default::default()
    }),
    ty: project_type(ty),
    link: project_link(link),
    siblings: Some(HashMap::new()),
    profiles: Some(HashMap::from([
      (
        "debug".to_string(),
        Profile {
          c_flags: Some(Flags::Append(vec![
            "-g".to_string(),
            "-O0".to_string(),
          ])),
          ld_flags: Some(Flags::Append(vec![
            "-g".to_string(),
            "-O0".to_string(),
          ])),
          ..Default::default()
        },
      ),
      (
        "release".to_string(),
        Profile {
          c_flags: Some(Flags::Append(vec!["-O2".to_string()])),
          ld_flags: Some(Flags::Append(vec![
            "-O2".to_string(),
            "-flto".to_string(),
          ])),
          ..Default::default()
        },
      ),
    ])),
    dependencies: Some(HashMap::new()),
    tests: Some(HashMap::new()),
    output: Some(Output::default()),
    ..Default::default()
  }
}

/// Best-effort build-system detection for an added dependency, by marker file.
fn guess_build_system(dir: &Path) -> Option<BuildSystem> {
  for (marker, system) in [
    ("CMakeLists.txt", BuildSystem::Cmake),
    ("meson.build", BuildSystem::Meson),
    ("configure.ac", BuildSystem::Autotools),
    ("configure", BuildSystem::Autotools),
    ("GNUmakefile", BuildSystem::Make),
    ("makefile", BuildSystem::Make),
    ("justfile", BuildSystem::Just),
    ("Justfile", BuildSystem::Just),
    ("Makefile", BuildSystem::Make),
    ("xmake.lua", BuildSystem::Xmake),
    ("build.ninja", BuildSystem::Ninja),
  ] {
    if dir.join(marker).exists() {
      return Some(system);
    }
  }
  None
}

/// Re-apply the quoting KDL parsing discarded.
///
/// `kdl` does not preserve value quoting, so a rewritten manifest would turn a
/// deliberately-quoted `"c"` into bare `c`. `original` is the pre-edit text;
/// every word that was quoted there is re-quoted in `out`.
fn restore_quotes(original: &str, out: &str) -> String {
  let quoted: HashSet<&str> = original.split('"').skip(1).step_by(2).collect();
  if quoted.is_empty() {
    return out.to_string();
  }

  let mut res = String::with_capacity(out.len() + 8);
  for line in out.lines() {
    let lead = line.len() - line.trim_start().len();
    res.push_str(&line[..lead]);
    let mut first = true;
    for (i, seg) in line[lead..].split('"').enumerate() {
      if i % 2 == 1 {
        res.push('"');
        res.push_str(seg);
        res.push('"');
        first = false;
        continue;
      }
      for tok in seg.split_whitespace() {
        if !first && quoted.contains(tok) {
          res.push('"');
          res.push_str(tok);
          res.push('"');
        } else {
          res.push_str(tok);
        }
        res.push(' ');
        first = false;
      }
    }
    if res.ends_with(' ') {
      res.pop();
    }
    res.push('\n');
  }
  res
}

/// Autoformat an edited `conjure.kdl`, restore its quoting, and write it back.
fn save_edit(text: &str, doc: &mut kdl::KdlDocument) -> Result<()> {
  let cfg = FormatConfigBuilder::new().indent("  ").build();
  doc.autoformat_config(&cfg);
  let out = fix_braces(&doc.to_string());
  fs::write("conjure.kdl", restore_quotes(text, &out)).into_diagnostic()?;
  Ok(())
}

fn dependency_exists(name: &str) -> Result<bool> {
  let text = std::fs::read_to_string("conjure.kdl").into_diagnostic()?;
  let doc: KdlDocument = text.parse().into_diagnostic()?;
  Ok(
    doc
      .get("project")
      .and_then(|p| p.children())
      .and_then(|c| c.get("dependencies"))
      .and_then(|d| d.children())
      .is_some_and(|deps| deps.get(name).is_some()),
  )
}

/// Insert or replace a dependency node in `conjure.kdl`, building the node from
/// the CLI args and round-tripping the file through [`save_edit`].
fn edit_dependencies(
  args: &AddArgs,
  name: &str,
  build: Option<String>,
  transport: Option<String>,
) -> Result<()> {
  let text = std::fs::read_to_string("conjure.kdl").into_diagnostic()?;
  let mut doc: kdl::KdlDocument = text.parse().into_diagnostic()?;

  let project = doc.get_mut("project").ok_or_else(|| {
    miette::miette!(
      help = "Generate a conjure.kdl using `conjure init`",
      "No project node in conjure.kdl"
    )
  })?;
  if project.children_mut().is_none() {
    *project.children_mut() = Some(kdl::KdlDocument::new());
  }
  let project_children = project.children_mut().as_mut().unwrap();

  if project_children.get_mut("dependencies").is_none() {
    let mut n = kdl::KdlNode::new("dependencies");
    n.set_children(kdl::KdlDocument::new());
    project_children.nodes_mut().push(n);
  }
  let deps = project_children.get_mut("dependencies").unwrap();
  if deps.children_mut().is_none() {
    *deps.children_mut() = Some(kdl::KdlDocument::new());
  }
  let children = deps.children_mut().as_mut().unwrap();

  miette::ensure!(
    args.force || children.get(name).is_none(),
    miette::miette!(
      help = "Use --force to replace",
      "Dependency `{name}` already exists"
    )
  );

  if children.get(name).is_none() {
    children.nodes_mut().retain(|n| n.name().value() != name);
  }

  let mut frag = format!("{name} {{");
  match &args.local_or_remote {
    LocalOrRemote::Local => {
      frag.push_str(&format!("\n  local \"{}\"", args.path_or_url))
    }
    host => frag.push_str(&format!(
      "\n  remote {} \"{}\"",
      host.to_possible_value().unwrap().get_name(),
      args.path_or_url,
    )),
  }

  if let Some(r) = &args.r#ref {
    frag.push_str(&format!("\n  ref {r}"));
  }

  if let Some(t) = &transport {
    frag.push_str(&format!("\n  transport {t}"));
  }

  if let Some(b) = &build {
    frag.push_str(&format!("\n  build {b}"));
  }

  frag.push_str("\n}");
  let dep_node: KdlNode = frag.parse().into_diagnostic()?;
  children.nodes_mut().push(dep_node);

  save_edit(&text, &mut doc)?;
  Ok(())
}

/// Clone (or reuse) a remote dependency and record its resolved commit in the
/// lockfile. `fetch` selects `conjure update`'s always-refetch behavior.
fn lock_remote(
  ui: &Ui,
  lock: &mut lock::LockFile,
  name: &str,
  dep: &proj_parse::Dependency,
  fetch: bool,
) -> Result<()> {
  let (host, path) = match dep.remote.as_ref() {
    Some(proj_parse::Remote::Codeberg(p)) => ("codeberg", p),
    Some(proj_parse::Remote::GitHub(p)) => ("github", p),
    Some(proj_parse::Remote::BitBucket(p)) => ("bitbucket", p),
    Some(proj_parse::Remote::Git(p)) => ("git", p),
    None => unreachable!(),
  };

  let t = match dep.transport {
    Some(proj_parse::Transport::Ssh) | None => "ssh",
    Some(proj_parse::Transport::Https) => "https",
  };

  let dir = if fetch {
    git::clone_remote(Some(ui), host, path, t, name)?
  } else {
    git::ensure_cloned(Some(ui), &git::remote_url(host, path, t), name)?
  };

  if let Some(r) = &dep.r#ref {
    git::checkout(&dir, r)?;
  }

  let commit = git::resolve_head(&dir)?;
  let r#ref = git::head_ref(&dir)?;
  git::checkout(&dir, &commit)?;

  lock.lock(
    name,
    Some(dep.remote.clone().unwrap()),
    dep.transport,
    r#ref,
    commit,
  );
  Ok(())
}

/// Whether the current manifest declares any profiles (used to hide the `as`
/// subcommand when it would be useless).
fn has_profiles() -> bool {
  Project::from_file("conjure.kdl")
    .ok()
    .and_then(|p| p.profiles)
    .is_some_and(|p| !p.is_empty())
}

fn handle_new(args: &NewArgs) -> Result<()> {
  let proj_path = Path::new(".").join(slugify(&args.name));
  let ui = Ui::new();
  if proj_path.is_dir() {
    miette::ensure!(
      args.force,
      miette::miette!(
        help = "Use --force to overwrite the project",
        "Project `{}` already exists",
        args.name
      )
    );
  }

  let format = created_summary(
    &args.name,
    args.language.clone(),
    args.standard.clone(),
    args.ty.clone(),
    args.link.clone(),
    args.arch.clone(),
  );

  proj::make_proj(
    proj_path,
    build_project(
      args.name.clone(),
      args.language.clone(),
      args.standard.clone(),
      args.ty.clone(),
      args.link.clone(),
      args.arch.clone(),
    ),
  )?;

  ui.println(Some(&StepStatus::Success), format)?;
  Ok(())
}

fn handle_init(args: &InitArgs) -> Result<()> {
  let proj_path = Path::new(".");
  let ui = Ui::new();
  let proj_name = args.name.clone().unwrap_or_else(|| {
    env::current_dir()
      .ok()
      .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
      .unwrap_or_default()
  });

  if proj_path.join("project.kdl").exists() {
    miette::ensure!(
      args.force,
      miette::miette!(
        help = "Use --force to overwrite the project",
        "Project `{proj_name}` already exists"
      )
    );
  }

  let format = created_summary(
    &proj_name,
    args.language.clone(),
    args.standard.clone(),
    args.ty.clone(),
    args.link.clone(),
    args.arch.clone(),
  );

  proj::make_proj(
    proj_path,
    build_project(
      proj_name,
      args.language.clone(),
      args.standard.clone(),
      args.ty.clone(),
      args.link.clone(),
      args.arch.clone(),
    ),
  )?;

  ui.println(Some(&StepStatus::Success), format)?;
  Ok(())
}

fn handle_add(args: &AddArgs) -> Result<()> {
  let name = args
    .path_or_url
    .rsplit('/')
    .next()
    .unwrap_or(&args.path_or_url)
    .to_string();

  miette::ensure!(
    args.force || !dependency_exists(&name)?,
    miette::miette!(
      help = "Use --force to replace the dependency",
      "Dependency `{name}` already exists"
    )
  );

  let transport = args.transport.clone().unwrap_or(Transport::Ssh);
  let ui = Ui::new();

  let (dir, build, transport_name) = match &args.local_or_remote {
    LocalOrRemote::Local => (None, None, None),
    host => {
      let host_val = &host.to_possible_value().unwrap();
      let host_name = host_val.get_name();
      let t = transport
        .to_possible_value()
        .unwrap()
        .get_name()
        .to_string();
      let dir =
        git::clone_remote(Some(&ui), host_name, &args.path_or_url, &t, &name)?;
      let build = args
        .build
        .as_ref()
        .map(|b| b.to_possible_value().unwrap().get_name().to_string())
        .or_else(|| {
          guess_build_system(&dir)
            .map(|b| b.to_possible_value().unwrap().get_name().to_string())
        });
      (Some(dir), build, Some(t))
    }
  };

  edit_dependencies(args, &name, build, transport_name.clone())?;

  if let Some(dir) = dir {
    if let Some(r) = &args.r#ref {
      git::checkout(&dir, r)?;
    }
    let commit = git::resolve_head(&dir)?;
    let r#ref = git::head_ref(&dir)?.or_else(|| args.r#ref.clone());
    let remote = Some(match &args.local_or_remote {
      LocalOrRemote::Codeberg => {
        proj_parse::Remote::Codeberg(args.path_or_url.clone())
      }
      LocalOrRemote::Github => {
        proj_parse::Remote::GitHub(args.path_or_url.clone())
      }
      LocalOrRemote::Bitbucket => {
        proj_parse::Remote::BitBucket(args.path_or_url.clone())
      }
      LocalOrRemote::Git => proj_parse::Remote::Git(args.path_or_url.clone()),
      LocalOrRemote::Local => unreachable!(),
    });

    let transport = transport_name
      .map(|s| {
        proj_parse::Transport::try_from(s).map_err(|e| miette::miette!(e))
      })
      .transpose()?;

    let mut lock = lock::LockFile::load("conjure.lock")?;
    lock.lock(&name, remote, transport, r#ref, commit);
    lock.save("conjure.lock")?;
  }

  ui.println(
    Some(&StepStatus::Success),
    format!("Added dependency `{name}`"),
  )?;
  Ok(())
}

fn handle_remove(args: &RemoveArgs) -> Result<()> {
  let text = fs::read_to_string("conjure.kdl").into_diagnostic()?;
  let mut doc: KdlDocument = text.parse().into_diagnostic()?;
  let ui = Ui::new();

  let project = doc.get_mut("project").ok_or_else(|| {
    miette::miette!(
      help = "Use `conjure init` to generate a new conjure.kdl",
      "No project node in conjure.kdl"
    )
  })?;

  let project_children = project
    .children_mut()
    .as_mut()
    .ok_or_else(|| miette::miette!(
      help = "Verify the conjure.kdl to make sure nothing is wrong, try to generate a new one using `conjure init`", 
      "No children in project node, is your project malformed?"
    ))?;

  let deps = project_children.get_mut("dependencies").ok_or_else(|| {
    miette::miette!("No dependency node in project, nothing to remove")
  })?;

  let deps_children = deps.children_mut().as_mut().ok_or_else(|| {
    miette::miette!("No children in dependencies node, nothing to remove")
  })?;

  miette::ensure!(
    deps_children.get(&args.name).is_some(),
    miette::miette!(
      help = "Double check if the dependency exists, if not, use `conjure add` to add it",
      "Dependency `{}` not found",
      args.name
    )
  );

  deps_children
    .nodes_mut()
    .retain(|n| n.name().value() != args.name);
  if deps_children.nodes().is_empty() {
    project_children
      .nodes_mut()
      .retain(|n| n.name().value() != "dependencies");
  }

  save_edit(&text, &mut doc)?;

  if Path::new("conjure.lock").exists() {
    let mut lock = lock::LockFile::load("conjure.lock")?;
    lock.unlock(&args.name);
    lock.save("conjure.lock")?;
  }

  let dir = git::cache_dir().join(&args.name);
  if dir.is_dir() {
    fs::remove_dir_all(dir).into_diagnostic()?;
  }

  ui.println(
    Some(&StepStatus::Success),
    format!("Removed dependency `{}`", args.name),
  )?;
  Ok(())
}

fn handle_lock() -> Result<()> {
  let project = proj_parse::Project::from_file("conjure.kdl")?;
  let mut lock = lock::LockFile::load("conjure.lock")?;
  let ui = Ui::new();
  let mut deps_list: Vec<String> = vec![];

  lock.retain(|name| {
    project
      .dependencies
      .as_ref()
      .is_some_and(|deps| deps.contains_key(name))
  });

  if let Some(deps) = &project.dependencies {
    for (name, dep) in deps.iter().filter(|(_, d)| d.remote.is_some()) {
      deps_list.push(name.clone());
      ui.wrap(format!("Locking {name}"), || {
        lock_remote(&ui, &mut lock, name, dep, false)
      })?;
    }
  } else {
    ui.println(Some(&StepStatus::Failure), "No dependencies to lock")?;
    return Ok(());
  }

  lock.save("conjure.lock")?;
  ui.println(
    Some(&StepStatus::Success),
    format!("Locked dependencies {}", deps_list.join(", ")),
  )?;
  Ok(())
}

fn handle_update(args: &UpdateArgs) -> Result<()> {
  let project = proj_parse::Project::from_file("conjure.kdl")?;
  let mut lock = lock::LockFile::load("conjure.lock")?;
  let ui = Ui::new();

  let names: Vec<String> = lock
    .entries()
    .keys()
    .filter(|name| args.name.as_ref().is_none_or(|want| want == *name))
    .cloned()
    .collect();

  miette::ensure!(
    !names.is_empty(),
    miette::miette!(
      help = "Try to run `conjure lock` to lock the dependencies",
      "Dependencies are not locked"
    )
  );

  for name in names {
    let dep = project
      .dependencies
      .as_ref()
      .and_then(|d| d.get(&name))
      .ok_or_else(|| {
        miette::miette!("dependency `{name}` not found in conjure.kdl")
      })?;
    ui.wrap(format!("updating {name}"), || {
      lock_remote(&ui, &mut lock, &name, dep, true)
    })?;
  }
  lock.save("conjure.lock")?;
  ui.println(Some(&StepStatus::Success), "lockfile updated")?;
  Ok(())
}

fn handle_build(args: &BuildArgs) -> Result<()> {
  let project = Project::from_file("conjure.kdl")?;
  let profile = args
    .profile
    .clone()
    .or_else(|| std::env::var("CONJURE_PROFILE").ok());
  if let Some(name) = profile.as_deref() {
    miette::ensure!(
      project
        .profiles
        .as_ref()
        .is_some_and(|m| m.contains_key(name)),
      "profile `{name}` not found"
    );
  }

  build::build(
    &project,
    profile.as_deref(),
    args.force,
    !args.no_siblings,
    args.threads,
  )
}

fn handle_test(args: &TestArgs) -> Result<()> {
  let project = Project::from_file("conjure.kdl")?;
  let profile = args
    .profile
    .clone()
    .or_else(|| std::env::var("CONJURE_PROFILE").ok());
  if let Some(name) = profile.as_deref() {
    miette::ensure!(
      project
        .profiles
        .as_ref()
        .is_some_and(|m| m.contains_key(name)),
      "profile `{name}` not found"
    );
  }
  build::test(&project, profile.as_deref(), &args.names, args.threads)
}

fn handle_compile_commands(args: &CompileCommandsArgs) -> Result<()> {
  let project = Project::from_file("conjure.kdl")?;
  let ui = Ui::new();

  let profile = args
    .profile
    .clone()
    .or_else(|| std::env::var("CONJURE_PROFILE").ok());
  if let Some(name) = profile.as_deref() {
    miette::ensure!(
      project
        .profiles
        .as_ref()
        .is_some_and(|m| m.contains_key(name)),
      "profile `{name}` not found"
    );
  }

  compile::compile_commands(&project, profile.as_deref(), !args.no_siblings)?;
  ui.println(
    Some(&StepStatus::Success),
    "Generated compile_commands.json",
  )?;
  Ok(())
}

/// Re-enter the CLI with `CONJURE_PROFILE` set, so the wrapped subcommand runs
/// under `args.profile`. The profile must exist; `CONJURE_PROFILE` is how the
/// subcommand's `handle_*` will see it.
fn handle_as(args: AsArgs) -> Result<()> {
  let project = Project::from_file("conjure.kdl")?;
  let profiles = project
    .profiles
    .as_ref()
    .filter(|p| !p.is_empty())
    .ok_or_else(|| miette::miette!("no profiles in conjure.kdl"))?;
  miette::ensure!(
    profiles.contains_key(&args.profile),
    "profile `{}` not found",
    args.profile
  );

  unsafe {
    std::env::set_var("CONJURE_PROFILE", &args.profile);
  }

  run_command(&args.subcommand.into())
}

fn run_command(cmd: &Commands) -> Result<()> {
  match cmd {
    Commands::New(args) => handle_new(args)?,
    Commands::Init(args) => handle_init(args)?,
    Commands::Add(args) => handle_add(args)?,
    Commands::Rm(args) => handle_remove(args)?,
    Commands::Lock => handle_lock()?,
    Commands::Update(args) => handle_update(args)?,
    Commands::Build(args) => handle_build(args)?,
    Commands::Compile(args) => handle_compile_commands(args)?,
    Commands::Test(args) => handle_test(args)?,
    Commands::As(_) => unreachable!(),
  }
  Ok(())
}

fn parse_cli() -> Cli {
  let mut cmd = Cli::command();
  if !has_profiles() {
    cmd = cmd.mut_subcommand("as", |c| c.hide(true));
  }
  let matches = cmd.get_matches();
  Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// CLI entry point.
pub fn run() -> Result<(), miette::Error> {
  let cli = parse_cli();
  match cli.command {
    Commands::As(args) => handle_as(args),
    cmd => run_command(&cmd),
  }
}

#[cfg(test)]
mod tests {
  use super::restore_quotes;

  #[test]
  fn restore_quotes_keeps_quoted_words_and_bare_ones() {
    let original = "project {\n  name \"meow2\"\n  language \"c\"\n  compile {\n    cc \"\"\n  }\n  dependencies {\n    cheese {\n      transport ssh\n    }\n  }\n}";
    let out = "project {\n  name meow2\n  language c\n  compile {\n    cc \"\"\n  }\n  dependencies {\n    cheese {\n      transport ssh\n    }\n  }\n}";
    let expected = "project {\n  name \"meow2\"\n  language \"c\"\n  compile {\n    cc \"\"\n  }\n  dependencies {\n    cheese {\n      transport ssh\n    }\n  }\n}\n";
    assert_eq!(restore_quotes(original, out), expected);
  }
}
