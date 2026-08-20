use super::ui::Ui;
use indicatif::ProgressBar;
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

pub fn cache_dir() -> PathBuf {
  PathBuf::from(".conjure").join("deps")
}

fn creds<'a>() -> git2::RemoteCallbacks<'a> {
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

    Err(git2::Error::from_str("No supported credential method"))
  });
  c
}

fn fetch_options<'a>(pb: Option<&'a ProgressBar>) -> git2::FetchOptions<'a> {
  let mut cbs = creds();
  if let Some(pb) = pb {
    cbs.transfer_progress(move |stats| {
      let received = stats.received_objects();
      let total = stats.total_objects();

      if total > 0 {
        pb.set_length(total as u64);
      }

      pb.set_position(received as u64);
      true
    });
  }

  let mut fo = git2::FetchOptions::new();
  fo.remote_callbacks(cbs);
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

pub fn ensure_cloned(ui: Option<&Ui>, url: &str, name: &str) -> Result<PathBuf> {
  let dir = cache_dir().join(name);
  if !dir.is_dir() {
    fs::create_dir_all(dir.parent().unwrap()).into_diagnostic()?;
    let pb = ui.map(|u| u.bar(0, format!("Cloning {name}"), "objects"));

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_options(pb.as_ref()));
    let result = builder.clone(url, &dir);
    if let Some(pb) = &pb {
      match &result {
        Ok(_) => pb.finish_with_message(format!("✓ Cloned {name}")),
        Err(_) => pb.finish_with_message(format!("✗ Failed to clone {name}")),
      }
    }
    result.into_diagnostic()?;
  }
  Ok(dir)
}

pub fn clone(ui: Option<&Ui>, url: &str, name: &str) -> Result<PathBuf> {
  let dir = ensure_cloned(ui, url, name)?;
  fetch(ui, url, &dir)?;
  Ok(dir)
}

pub fn clone_remote(
  ui: Option<&Ui>,
  host: &str,
  path: &str,
  transport: &str,
  name: &str,
) -> Result<PathBuf> {
  clone(ui, &remote_url(host, path, transport), name)
}

fn fetch(ui: Option<&Ui>, url: &str, dir: &Path) -> Result<()> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  let mut remote = repo
    .find_remote("origin")
    .or_else(|_| repo.remote("origin", url))
    .into_diagnostic()?;

  let pb = ui.map(|u| {
    u.bar(
      0,
      format!("Fetching {}", dir.file_name().unwrap().to_string_lossy()),
      "objects",
    )
  });
  let mut fo = fetch_options(pb.as_ref());
  let result = remote.fetch(
    &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"],
    Some(&mut fo),
    None,
  );

  if let Some(pb) = &pb {
    match &result {
      Ok(_) => pb.finish_with_message(format!("✓ Fetched {}", dir.display())),
      Err(_) => pb.finish_with_message(format!("✗ Failed to fetch {}", dir.display())),
    }
  }
  result.into_diagnostic()?;
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
