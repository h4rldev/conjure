use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
};

const CACHE_FILE: &str = ".conjure/build/deps.kdl";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DepCache {
  projects: HashMap<String, HashMap<String, CachedDep>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CachedDep {
  key: String,
  lib: PathBuf,
}

impl DepCache {
  pub fn load(base: &Path) -> Self {
    fs::read_to_string(base.join(CACHE_FILE))
      .ok()
      .and_then(|s| kdl::de::from_str(&s).ok())
      .unwrap_or_default()
  }

  pub fn save(&self, base: &Path) {
    let path = base.join(CACHE_FILE);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut doc) = kdl::se::to_document(self) {
      let cfg = kdl::FormatConfigBuilder::new().indent("  ").build();
      doc.autoformat_config(&cfg);

      let text = super::proj_write::fix_braces(&doc.to_string());
      let _ = fs::write(path, text);
    }
  }
  pub fn hit(&self, scope: &str, name: &str, key: &str) -> Option<PathBuf> {
    self
      .projects
      .get(scope)?
      .get(name)
      .filter(|c| c.key == key && c.lib.exists())
      .map(|c| c.lib.clone())
  }

  pub fn insert(&mut self, scope: &str, name: &str, key: String, lib: PathBuf) {
    self
      .projects
      .entry(scope.to_string())
      .or_default()
      .insert(name.to_string(), CachedDep { key, lib });
  }
}
