use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

const CACHE_FILE: &str = ".conjure/build/deps.kdl";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DepCache {
  deps: HashMap<String, CachedDep>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CachedDep {
  commit: String,
  lib: PathBuf,
}

impl DepCache {
  pub fn load() -> Self {
    fs::read_to_string(CACHE_FILE)
      .ok()
      .and_then(|s| kdl::de::from_str(&s).ok())
      .unwrap_or_default()
  }

  pub fn save(&self) {
    let _ = fs::create_dir_all(".conjure/build");
    if let Ok(mut doc) = kdl::se::to_document(self) {
      let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
      doc.autoformat_config(&cfg);

      let text = super::proj_write::fix_braces(&doc.to_string());
      let _ = fs::write(CACHE_FILE, text);
    }
  }

  pub fn hit(&self, name: &str, commit: &str) -> Option<PathBuf> {
    self
      .deps
      .get(name)
      .filter(|c| c.commit == commit)
      .map(|c| c.lib.clone())
  }

  pub fn insert(&mut self, name: &str, commit: String, lib: PathBuf) {
    self
      .deps
      .insert(name.to_string(), CachedDep { commit, lib });
  }
}
