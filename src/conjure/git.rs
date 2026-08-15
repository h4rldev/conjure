use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

pub fn cache_dir() -> PathBuf {
  PathBuf::from(".conjure").join("cache").join("deps")
}

fn creds() -> git2::RemoteCallbacks<'static> {
  let mut c = git2::RemoteCallbacks::new();
  c.credentials(|_url, username, allowed| {
    if allowed.contains(git2::CredentialType::SSH_KEY) {
      return git2::Cred::ssh_key_from_agent(username.unwrap_or("git"));
    }

    if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
      let user = std::env::var("GIT_USER").unwrap_or_default();
      let token =
        std::env::var("GIT_TOKEN").map_err(|_| git2::Error::from_str("GIT_TOKEN not set"))?;
      return git2::Cred::userpass_plaintext(&user, &token);
    }

    Err(git2::Error::from_str("no supported credential method"))
  });
  c
}

fn fetch_options() -> git2::FetchOptions<'static> {
  let mut fo = git2::FetchOptions::new();
  fo.remote_callbacks(creds());
  fo
}

pub fn remote_url(host: &str, path: &str, transport: &str) -> String {
  let base = match host {
    "codeberg" => "codeberg.org",
    "github" => "github.com",
    "bitbucket" => "bitbucket.org",
    _ => path,
  };

  match transport {
    "https" => format!("https://{base}/{path}.git"),
    _ => format!("git@{base}:{path}.git"),
  }
}

pub fn clone_remote(host: &str, path: &str, transport: &str, name: &str) -> Result<PathBuf> {
  clone(&remote_url(host, path, transport), name)
}

pub fn ensure_cloned(url: &str, name: &str) -> Result<PathBuf> {
  let dir = cache_dir().join(name);
  if !dir.is_dir() {
    fs::create_dir_all(dir.parent().unwrap()).into_diagnostic()?;
    let mut builder = git2::build::RepoBuilder::new();

    builder.fetch_options(fetch_options());
    println!("cloning {url} to {dir:?}");
    builder.clone(url, &dir).into_diagnostic()?;
  }
  Ok(dir)
}

pub fn clone(url: &str, name: &str) -> Result<PathBuf> {
  let dir = ensure_cloned(url, name)?;
  fetch(url, &dir)?;
  Ok(dir)
}

fn fetch(url: &str, dir: &Path) -> Result<()> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  let mut remote = repo
    .find_remote("origin")
    .or_else(|_| repo.remote("origin", url))
    .into_diagnostic()?;

  remote
    .fetch(
      &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"],
      Some(&mut fetch_options()),
      None,
    )
    .into_diagnostic()?;

  Ok(())
}

pub fn resolve_head(dir: &Path) -> Result<String> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  Ok(
    repo
      .head()
      .into_diagnostic()?
      .peel_to_commit()
      .into_diagnostic()?
      .id()
      .to_string(),
  )
}

pub fn head_ref(dir: &Path) -> Result<Option<String>> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  Ok(
    repo
      .head()
      .ok()
      .and_then(|h| h.shorthand().ok().map(|s| s.to_string())),
  )
}

pub fn checkout(dir: &Path, commit: &str) -> Result<()> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  let obj = repo.revparse_single(commit).into_diagnostic()?;
  repo
    .reset(&obj, git2::ResetType::Hard, None)
    .into_diagnostic()?;
  Ok(())
}
