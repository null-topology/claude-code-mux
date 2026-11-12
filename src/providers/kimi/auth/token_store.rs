use serde::{Deserialize, Serialize};

use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(rename = "userId", default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

pub struct KimiTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> KimiTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

pub fn file_store() -> KimiTokenStore<FileAuthStore<StoredAuth>> {
    let deps = paths::DirResolverEnv::default();
    let primary = paths::kimi_auth_file(&deps);
    let legacy = paths::legacy_config_dir(&deps)
        .join("kimi")
        .join("auth.json");
    let store = FileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
    );
    KimiTokenStore::new(store)
}
