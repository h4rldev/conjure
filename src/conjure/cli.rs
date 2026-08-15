use super::{
  build, git, lock, proj,
  proj_parse::{self, Compile, Flags, Profile, Project, ProjectType, slugify},
  proj_write::fix_braces,
};

use clap::{
  Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
  builder::{Styles, styling::AnsiColor},
};
use kdl::{FormatConfigBuilder, KdlDocument, KdlNode};
use miette::{IntoDiagnostic, Result};
use std::{
  collections::{HashMap, HashSet},
  env, fs,
  path::Path,
};

const LONG_ABOUT: &str = r#"
Conjure, the modern build-tool for C and C++.
Made by h4rl, for everyone.
"#;

const HELP_TEMPLATE: &str = "{about}\n{usage-heading} {usage}\n\n{all-args}";

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

#[derive(Subcommand)]
enum Commands {
  /// Create a new conjure project
  New(NewArgs),
  /// Initialize a conjure project in the current directory
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
  /// Run a subcommand as a specific profile
  As(AsArgs),
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
  #[arg(long, short = 'p')]
  profile: Option<String>,
}

#[derive(Args)]
struct AsArgs {
  profile: String,
  #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
  subcommand: Vec<String>,
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

#[derive(Clone, ValueEnum)]
enum Language {
  #[value(alias = "c")]
  C,
  #[value(alias = "c++")]
  Cpp,
}

#[derive(Clone, ValueEnum)]
enum TypeType {
  #[value(alias = "binary")]
  Binary,
  #[value(alias = "library")]
  Library,
}

#[derive(Clone, ValueEnum)]
enum LinkType {
  #[value(alias = "static")]
  Static,
  #[value(alias = "dynamic")]
  Dynamic,
}

fn project_type(ty: Option<TypeType>, link: Option<LinkType>) -> ProjectType {
  match (
    ty.unwrap_or(TypeType::Binary),
    link.unwrap_or(LinkType::Dynamic),
  ) {
    (TypeType::Binary, LinkType::Static) => ProjectType::BinaryStatic,
    (TypeType::Binary, LinkType::Dynamic) => ProjectType::BinaryDynamic,
    (TypeType::Library, LinkType::Static) => ProjectType::LibraryStatic,
    (TypeType::Library, LinkType::Dynamic) => ProjectType::LibraryDynamic,
  }
}

fn default_standard(language: Option<Language>) -> &'static str {
  match language {
    Some(Language::Cpp) => "c++17",
    _ => "c11",
  }
}

fn build_project(
  name: String,
  language: Option<Language>,
  standard: Option<String>,
  ty: Option<TypeType>,
  link: Option<LinkType>,
) -> Project {
  Project {
    name,
    language: match language {
      Some(Language::Cpp) => crate::conjure::proj_parse::Language::Cpp,
      _ => crate::conjure::proj_parse::Language::C,
    },
    compile: Some(Compile {
      standard: Some(standard.unwrap_or_else(|| default_standard(language).to_string())),
      ..Default::default()
    }),
    ty: project_type(ty, link),
    sub_projects: Some(HashMap::new()),
    profiles: Some(HashMap::from([
      (
        "debug".to_string(),
        Profile {
          c_flags: Some(Flags::Append(vec!["-g".to_string(), "-O0".to_string()])),
          ld_flags: Some(Flags::Append(vec!["-g".to_string(), "-O0".to_string()])),
        },
      ),
      (
        "release".to_string(),
        Profile {
          c_flags: Some(Flags::Append(vec!["-O2".to_string()])),
          ld_flags: Some(Flags::Append(vec!["-O2".to_string(), "-flto".to_string()])),
        },
      ),
    ])),
    dependencies: Some(HashMap::new()),
    ..Default::default()
  }
}

fn guess_build_system(dir: &Path) -> Option<BuildSystem> {
  for (marker, system) in [
    ("CMakeLists.txt", BuildSystem::Cmake),
    ("meson.build", BuildSystem::Meson),
    ("configure.ac", BuildSystem::Autotools),
    ("configure", BuildSystem::Autotools),
    ("GNUmakefile", BuildSystem::Make),
    ("makefile", BuildSystem::Make),
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

fn save_edit(text: &str, doc: &mut kdl::KdlDocument) -> Result<()> {
  let cfg = FormatConfigBuilder::new().indent("  ").build();
  doc.autoformat_config(&cfg);
  let out = fix_braces(&doc.to_string());
  fs::write("conjure.kdl", restore_quotes(text, &out)).into_diagnostic()?;
  Ok(())
}

fn edit_dependencies(
  args: &AddArgs,
  name: &str,
  build: Option<String>,
  transport: Option<String>,
) -> Result<()> {
  let text = std::fs::read_to_string("conjure.kdl").into_diagnostic()?;
  let mut doc: kdl::KdlDocument = text.parse().into_diagnostic()?;

  let project = doc
    .get_mut("project")
    .ok_or_else(|| miette::miette!("no project node in conjure.kdl"))?;
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
    "dependency `{name}` already exists; Use --force to replace",
  );

  if children.get(name).is_none() {
    children.nodes_mut().retain(|n| n.name().value() != name);
  }

  let mut frag = format!("{name} {{");
  match &args.local_or_remote {
    LocalOrRemote::Local => frag.push_str(&format!("\n  local \"{}\"", args.path_or_url)),
    host => frag.push_str(&format!(
      "\n  remote {} \"{}\"",
      host.to_possible_value().unwrap().get_name(),
      args.path_or_url,
    )),
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

fn lock_remote(
  lock: &mut lock::LockFile,
  name: &str,
  remote: &proj_parse::Remote,
  transport: Option<proj_parse::Transport>,
  fetch: bool,
) -> Result<()> {
  let (host, path) = match remote {
    proj_parse::Remote::Codeberg(p) => ("codeberg", p),
    proj_parse::Remote::GitHub(p) => ("github", p),
    proj_parse::Remote::BitBucket(p) => ("bitbucket", p),
    proj_parse::Remote::Git(p) => ("git", p),
  };

  let t = match transport {
    Some(proj_parse::Transport::Ssh) | None => "ssh",
    Some(proj_parse::Transport::Https) => "https",
  };

  let dir = if fetch {
    git::clone_remote(host, path, t, name)?
  } else {
    git::ensure_cloned(&git::remote_url(host, path, t), name)?
  };

  let commit = git::resolve_head(&dir)?;
  let r#ref = git::head_ref(&dir)?;
  git::checkout(&dir, &commit)?;

  lock.lock(name, Some(remote.clone()), transport, r#ref, commit);
  Ok(())
}

fn has_profiles() -> bool {
  Project::from_file("conjure.kdl")
    .ok()
    .and_then(|p| p.profiles)
    .is_some_and(|p| !p.is_empty())
}

fn handle_new(args: &NewArgs) -> Result<()> {
  let proj_path = Path::new(".").join(slugify(&args.name));
  if proj_path.is_dir() {
    miette::ensure!(
      args.force,
      "`{}` already exists; Use --force to overwrite",
      args.name
    );
  }

  proj::make_proj(
    proj_path,
    build_project(
      args.name.clone(),
      args.language.clone(),
      args.standard.clone(),
      args.ty.clone(),
      args.link.clone(),
    ),
  )
}

fn handle_init(args: &InitArgs) -> Result<()> {
  let proj_path = Path::new(".");
  let proj_name = args.name.clone().unwrap_or_else(|| {
    env::current_dir()
      .ok()
      .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
      .unwrap_or_default()
  });

  if proj_path.join("project.kdl").exists() {
    miette::ensure!(
      args.force,
      "`{proj_name}` already exists; Use --force to overwrite"
    );
  }

  proj::make_proj(
    proj_path,
    build_project(
      proj_name,
      args.language.clone(),
      args.standard.clone(),
      args.ty.clone(),
      args.link.clone(),
    ),
  )
}

fn handle_add(args: &AddArgs) -> Result<()> {
  let name = args
    .path_or_url
    .rsplit('/')
    .next()
    .unwrap_or(&args.path_or_url)
    .to_string();

  let transport = args.transport.clone().unwrap_or(Transport::Ssh);

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
      let dir = git::clone_remote(host_name, &args.path_or_url, &t, &name)?;
      let build = args
        .build
        .as_ref()
        .map(|b| b.to_possible_value().unwrap().get_name().to_string())
        .or_else(|| {
          guess_build_system(&dir).map(|b| b.to_possible_value().unwrap().get_name().to_string())
        });
      (Some(dir), build, Some(t))
    }
  };

  edit_dependencies(args, &name, build, transport_name.clone())?;

  if let Some(dir) = dir {
    let commit = git::resolve_head(&dir)?;
    let r#ref = git::head_ref(&dir)?;
    let remote = Some(match &args.local_or_remote {
      LocalOrRemote::Codeberg => proj_parse::Remote::Codeberg(args.path_or_url.clone()),
      LocalOrRemote::Github => proj_parse::Remote::GitHub(args.path_or_url.clone()),
      LocalOrRemote::Bitbucket => proj_parse::Remote::BitBucket(args.path_or_url.clone()),
      LocalOrRemote::Git => proj_parse::Remote::Git(args.path_or_url.clone()),
      LocalOrRemote::Local => unreachable!(),
    });

    let transport = transport_name
      .map(|s| proj_parse::Transport::try_from(s).map_err(|e| miette::miette!(e)))
      .transpose()?;

    let mut lock = lock::LockFile::load("conjure.lock")?;
    lock.lock(&name, remote, transport, r#ref, commit);
    lock.save("conjure.lock")?;
  }

  Ok(())
}

fn handle_remove(args: &RemoveArgs) -> Result<()> {
  let text = fs::read_to_string("conjure.kdl").into_diagnostic()?;
  let mut doc: KdlDocument = text.parse().into_diagnostic()?;

  let project = doc
    .get_mut("project")
    .ok_or_else(|| miette::miette!("No project node in conjure.kdl, run conjure init first"))?;

  let project_children = project
    .children_mut()
    .as_mut()
    .ok_or_else(|| miette::miette!("No children in project node, is your project malformed?"))?;

  let deps = project_children
    .get_mut("dependencies")
    .ok_or_else(|| miette::miette!("No dependency node in project, nothing to remove"))?;

  let deps_children = deps
    .children_mut()
    .as_mut()
    .ok_or_else(|| miette::miette!("No children in dependencies node, nothing to remove"))?;

  miette::ensure!(
    deps_children.get(&args.name).is_some(),
    "dependency `{}` not found",
    args.name
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

  Ok(())
}

fn handle_lock() -> Result<()> {
  let project = proj_parse::Project::from_file("conjure.kdl")?;
  let mut lock = lock::LockFile::load("conjure.lock")?;

  lock.retain(|name| {
    project
      .dependencies
      .as_ref()
      .is_some_and(|deps| deps.contains_key(name))
  });

  if let Some(deps) = &project.dependencies {
    for (name, dep) in deps.iter().filter(|(_, d)| d.remote.is_some()) {
      lock_remote(
        &mut lock,
        name,
        dep.remote.as_ref().unwrap(),
        dep.transport,
        false,
      )?;
    }
  }

  lock.save("conjure.lock")?;
  Ok(())
}

fn handle_update(args: &UpdateArgs) -> Result<()> {
  let mut lock = lock::LockFile::load("conjure.lock")?;

  let entries: Vec<_> = lock
    .entries()
    .iter()
    .filter(|(name, d)| d.remote.is_some() && args.name.as_ref().is_none_or(|want| want == *name))
    .map(|(name, d)| (name.clone(), d.remote.clone().unwrap(), d.transport))
    .collect();

  miette::ensure!(
    args.name.is_none() || !entries.is_empty(),
    "dependency `{}` is not locked",
    args.name.as_ref().unwrap()
  );

  for (name, remote, transport) in entries {
    lock_remote(&mut lock, &name, &remote, transport, true)?;
  }

  lock.save("conjure.lock")?;
  Ok(())
}

fn handle_build(args: &BuildArgs) -> Result<()> {
  let project = Project::from_file("conjure.kdl")?;
  let profile_name = args
    .profile
    .clone()
    .or_else(|| std::env::var("CONJURE_PROFILE").ok());
  let profile = match profile_name.as_deref() {
    Some(name) => {
      let profiles = project
        .profiles
        .as_ref()
        .ok_or_else(|| miette::miette!("no profiles in conjure.kdl"))?;
      let p = profiles
        .get(name)
        .ok_or_else(|| miette::miette!("profile `{name}` not found"))?;
      Some((name, p))
    }
    None => None,
  };

  build::build(&project, profile)
}

fn handle_as(args: &AsArgs) -> Result<()> {
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
    std::env::set_var("CONJURE_PROFILE", args.profile.clone());
  }

  let argv0 = std::env::args().next().unwrap_or_else(|| "conjure".into());
  let inner = Cli::parse_from([argv0].into_iter().chain(args.subcommand.iter().cloned()));
  run_command(&inner.command)
}

fn parse_cli() -> Cli {
  let mut cmd = Cli::command();
  if !has_profiles() {
    cmd = cmd.mut_subcommand("as", |c| c.hide(true));
  }
  let matches = cmd.get_matches();
  Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
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
    Commands::As(_) => unreachable!(),
  }
  Ok(())
}

pub fn run() -> Result<(), miette::Error> {
  let cli = parse_cli();

  if let Commands::As(args) = &cli.command {
    return handle_as(args);
  }

  run_command(&cli.command)
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
