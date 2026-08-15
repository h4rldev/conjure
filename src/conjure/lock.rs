use super::{
  proj_parse::{Remote, Transport},
  proj_write::fix_braces,
};
use miette::{IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct LockFile {
  lock: HashMap<String, LockedDep>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LockedDep {
  pub remote: Option<Remote>,
  pub transport: Option<Transport>,
  #[serde(rename = "ref")]
  pub r#ref: Option<String>,
  pub commit: String,
}

impl LockFile {
  pub fn load(path: impl AsRef<Path>) -> Result<Self> {
    match std::fs::read_to_string(path) {
      Ok(text) => Ok(kdl::de::from_str(&text).into_diagnostic()?),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
      Err(e) => Err(e).into_diagnostic(),
    }
  }

  pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
    let mut doc = kdl::se::to_document(self).into_diagnostic()?;
    let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
    doc.autoformat_config(&cfg);

    let text = fix_braces(&doc.to_string());
    std::fs::write(path, text).into_diagnostic()?;
    Ok(())
  }

  pub fn lock(
    &mut self,
    name: &str,
    remote: Option<Remote>,
    transport: Option<Transport>,
    r#ref: Option<String>,
    commit: String,
  ) {
    self.lock.insert(
      name.to_string(),
      LockedDep {
        remote,
        transport,
        r#ref,
        commit,
      },
    );
  }

  pub fn unlock(&mut self, name: &str) {
    self.lock.remove(name);
  }

  pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
    self.lock.retain(|k, _| f(k));
  }
  pub fn entries(&self) -> &HashMap<String, LockedDep> {
    &self.lock
  }
}
