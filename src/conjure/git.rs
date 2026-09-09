//! Git access, via the `git2` (libgit2) crate rather than shelling out to the
//! system `git` binary. libgit2 keeps clone, fetch, and checkout in-process,
//! avoiding a dependency on an external tool and its credential/version quirks.
//!
//! Clones are "ensure"-style: [`ensure_cloned`]/[`ensure_cloned_at`] reuse an
//! existing directory and only clone on a miss, so callers can invoke them
//! freely. They land under the cache dir (`.conjure/deps` by default; `deps.rs`
//! layers a per-scope subdirectory on top so co-built projects don't collide).
//! Refetching an existing clone only happens through [`clone`]/[`clone_remote`]
//! (`conjure update`); a normal build reuses the checkout and resets it to the
//! locked commit via [`checkout`].
//!
//! Credentials come from the environment only: the ssh-agent for SSH, and
//! `GIT_USER` + `GIT_TOKEN` for HTTPS. There is no interactive prompt.

/***********************************************************************/

use super::ui::Ui;
use indicatif::ProgressBar;
use miette::{IntoDiagnostic, Result};
use std::{
  fs,
  path::{Path, PathBuf},
};

/***********************************************************************/

/// Callbacks resolving credentials for a fetch: ssh-agent for SSH, then
/// `GIT_USER`/`GIT_TOKEN` for plaintext auth. A fetch needing a method we can't
/// supply fails rather than prompting.
fn creds<'a>() -> git2::RemoteCallbacks<'a> {
  let mut c = git2::RemoteCallbacks::new();
  c.credentials(|_url, username, allowed| {
    if allowed.contains(git2::CredentialType::SSH_KEY) {
      return git2::Cred::ssh_key_from_agent(username.unwrap_or("git"));
    }

    if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
      let user = std::env::var("GIT_USER").unwrap_or_default();
      let token = std::env::var("GIT_TOKEN")
        .map_err(|_| git2::Error::from_str("GIT_TOKEN not set"))?;
      return git2::Cred::userpass_plaintext(&user, &token);
    }

    Err(git2::Error::from_str("No supported credential method"))
  });
  c
}

/// Fetch options carrying [`creds`] and, when a `pb` is given, a transfer
/// callback that drives it with the received/total object counts.
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

/// Refresh every branch and tag of an existing clone.
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
      Err(_) => {
        pb.finish_with_message(format!("✗ Failed to fetch {}", dir.display()))
      }
    }
  }
  result.into_diagnostic()?;
  Ok(())
}

/// Default clone location, relative to the invocation root.
pub fn cache_dir() -> PathBuf {
  PathBuf::from(".conjure").join("deps")
}

/// Build the clone URL for a known host and transport. An unrecognised host is
/// treated as the path itself, so `git` + a full URL works.
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

/// Clone `url` into `base/name` if that directory is absent, else reuse it.
///
/// `pb` is supplied by the caller rather than created here so that a batch of
pub fn ensure_cloned_at(
  base: &Path,
  url: &str,
  name: &str,
  pb: Option<&ProgressBar>,
) -> Result<PathBuf> {
  let dir = base.join(name);
  if dir.is_dir() {
    return Ok(dir);
  }
  fs::create_dir_all(base).into_diagnostic()?;

  let mut builder = git2::build::RepoBuilder::new();
  builder.fetch_options(fetch_options(pb));
  builder.clone(url, &dir).into_diagnostic()?;
  Ok(dir)
}

/// [`ensure_cloned_at`] under the default [`cache_dir`], with its own bar.
pub fn ensure_cloned(
  ui: Option<&Ui>,
  url: &str,
  name: &str,
) -> Result<PathBuf> {
  let pb = ui.map(|u| u.bar(0, format!("Cloning {name}"), "objects"));
  ensure_cloned_at(&cache_dir(), url, name, pb.as_ref())
}

/// Ensure a clone exists and then refetch it, so an existing cache is advanced.
pub fn clone(ui: Option<&Ui>, url: &str, name: &str) -> Result<PathBuf> {
  let dir = ensure_cloned(ui, url, name)?;
  fetch(ui, url, &dir)?;
  Ok(dir)
}

/// Clone (and refetch) a repository identified by host + path + transport.
pub fn clone_remote(
  ui: Option<&Ui>,
  host: &str,
  path: &str,
  transport: &str,
  name: &str,
) -> Result<PathBuf> {
  clone(ui, &remote_url(host, path, transport), name)
}

/// The commit id at `HEAD`, as a hex string.
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

/// The current branch name, or `None` when `HEAD` is detached.
pub fn head_ref(dir: &Path) -> Result<Option<String>> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  Ok(
    repo
      .head()
      .ok()
      .and_then(|h| h.shorthand().ok().map(|s| s.to_string())),
  )
}

/// Hard-reset the checkout to `commit`, discarding local changes. This is how a
/// checkout is pinned to the locked revision.
pub fn checkout(dir: &Path, commit: &str) -> Result<()> {
  let repo = git2::Repository::open(dir).into_diagnostic()?;
  let obj = repo.revparse_single(commit).into_diagnostic()?;
  repo
    .reset(&obj, git2::ResetType::Hard, None)
    .into_diagnostic()?;
  Ok(())
}
