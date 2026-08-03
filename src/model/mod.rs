//! Supported models and secure local credential storage.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    OpenAi,
    DeepSeek,
    Qwen,
    Kimi,
}

impl Model {
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
        }
    }

    pub const fn base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::DeepSeek => "https://api.deepseek.com",
            Self::Qwen => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Self::Kimi => "https://api.moonshot.cn/v1",
        }
    }

    pub const fn default_model_id(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o",
            Self::DeepSeek => "deepseek-v4-flash",
            Self::Qwen => "qwen3-coder-plus",
            Self::Kimi => "kimi-k3",
        }
    }

    pub const fn api_key_label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI API key",
            Self::DeepSeek => "DeepSeek API key",
            Self::Qwen => "Qwen API key",
            Self::Kimi => "Kimi API key",
        }
    }

    pub const fn api_key_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::Qwen => "DASHSCOPE_API_KEY",
            Self::Kimi => "MOONSHOT_API_KEY",
        }
    }

    pub const fn supports_custom_temperature(self) -> bool {
        !matches!(self, Self::Kimi)
    }

    pub const fn context_window(self) -> Option<usize> {
        match self {
            Self::OpenAi | Self::DeepSeek => Some(128_000),
            Self::Qwen => Some(1_000_000),
            Self::Kimi => Some(256_000),
        }
    }

    pub const fn supported() -> &'static [&'static str] {
        &["openai", "deepseek", "qwen", "kimi"]
    }

    pub const fn all() -> &'static [Self] {
        &[Self::OpenAi, Self::DeepSeek, Self::Qwen, Self::Kimi]
    }
}

impl fmt::Display for Model {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

impl FromStr for Model {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::OpenAi),
            "deepseek" | "deep-seek" => Ok(Self::DeepSeek),
            "qwen" | "dashscope" => Ok(Self::Qwen),
            "kimi" | "moonshot" => Ok(Self::Kimi),
            unsupported => Err(format!(
                "unsupported model '{unsupported}'; supported models: {}",
                Self::supported().join(", ")
            )),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    active_model: Option<Model>,
    #[serde(default)]
    models: BTreeMap<String, ModelCredential>,
}

#[derive(Serialize, Deserialize)]
struct ModelCredential {
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelStatus {
    pub model: Model,
    pub logged_in: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn discover() -> Result<Self> {
        let directory = match std::env::var_os("NOYA_CONFIG_DIR") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot determine the user home directory")?
                .join("noya"),
        };
        Ok(Self::at(directory.join("credentials.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn login(&self, model: Model, api_key: &str, base_url: Option<&str>) -> Result<()> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            bail!("API key cannot be empty");
        }
        let base_url = base_url.map(str::trim).filter(|value| !value.is_empty());
        let mut credentials = self.load()?;
        credentials
            .models
            .entry(model.id().to_string())
            .and_modify(|credential| {
                credential.api_key = api_key.to_string();
                if let Some(base_url) = base_url {
                    credential.base_url = Some(base_url.to_string());
                }
            })
            .or_insert_with(|| ModelCredential {
                api_key: api_key.to_string(),
                base_url: base_url.map(str::to_string),
                model_id: None,
            });
        credentials.active_model = Some(model);
        self.save(&credentials)
    }

    pub fn logout(&self, model: Model) -> Result<bool> {
        let mut credentials = self.load()?;
        let removed = credentials.models.remove(model.id()).is_some();
        if credentials.active_model == Some(model) {
            credentials.active_model = None;
        }
        if removed {
            self.save(&credentials)?;
        }
        Ok(removed)
    }

    pub fn active_model(&self) -> Result<Option<Model>> {
        Ok(self.load()?.active_model)
    }

    pub fn api_key(&self, model: Model) -> Result<Option<String>> {
        Ok(self
            .load()?
            .models
            .get(model.id())
            .map(|credential| credential.api_key.clone()))
    }

    pub fn base_url(&self, model: Model) -> Result<Option<String>> {
        Ok(self
            .load()?
            .models
            .get(model.id())
            .and_then(|credential| credential.base_url.clone()))
    }

    fn model_id(&self, model: Model) -> Result<Option<String>> {
        Ok(self
            .load()?
            .models
            .get(model.id())
            .and_then(|credential| credential.model_id.clone()))
    }

    pub fn model_statuses(&self) -> Result<Vec<ModelStatus>> {
        let credentials = self.load()?;
        Ok(Model::all()
            .iter()
            .copied()
            .map(|model| ModelStatus {
                model,
                logged_in: credentials.models.contains_key(model.id()),
                active: credentials.active_model == Some(model),
            })
            .collect())
    }

    fn load(&self) -> Result<Credentials> {
        if !self.path.exists() {
            return Ok(Credentials::default());
        }
        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("read credentials from {}", self.path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("decode credentials from {}", self.path.display()))
    }

    fn save(&self, credentials: &Credentials) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("credential path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&self.path)
            .with_context(|| format!("open credential file {}", self.path.display()))?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let encoded = serde_json::to_vec_pretty(credentials)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }
}

#[derive(Default)]
pub struct ModelOverrides {
    pub model: Option<Model>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeModelConfig {
    pub model: Model,
    pub api_key: String,
    pub base_url: String,
    pub model_id: String,
}

impl RuntimeModelConfig {
    pub fn resolve(overrides: ModelOverrides, store: &CredentialStore) -> Result<Self> {
        let model = overrides
            .model
            .or(store.active_model()?)
            .unwrap_or(Model::OpenAi);
        let api_key = overrides
            .api_key
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var(model.api_key_env()).ok())
            .or(store.api_key(model)?)
            .unwrap_or_default();
        Ok(Self {
            model,
            api_key,
            base_url: overrides
                .base_url
                .or(store.base_url(model)?)
                .unwrap_or_else(|| model.base_url().to_string()),
            model_id: overrides
                .model_id
                .or(store.model_id(model)?)
                .unwrap_or_else(|| model.default_model_id().to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_uses_current_openai_compatible_defaults() {
        let model: Model = "deepseek".parse().unwrap();

        assert_eq!(model.base_url(), "https://api.deepseek.com");
        assert_eq!(model.default_model_id(), "deepseek-v4-flash");
        assert_eq!(model.api_key_label(), "DeepSeek API key");
    }

    #[test]
    fn qwen_and_kimi_use_openai_compatible_defaults() {
        let qwen: Model = "qwen".parse().unwrap();
        assert_eq!(
            qwen.base_url(),
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        assert_eq!(qwen.default_model_id(), "qwen3-coder-plus");
        assert_eq!(qwen.api_key_label(), "Qwen API key");
        assert_eq!(qwen.api_key_env(), "DASHSCOPE_API_KEY");

        let kimi: Model = "kimi".parse().unwrap();
        assert_eq!(kimi.base_url(), "https://api.moonshot.cn/v1");
        assert_eq!(kimi.default_model_id(), "kimi-k3");
        assert_eq!(kimi.api_key_label(), "Kimi API key");
        assert_eq!(kimi.api_key_env(), "MOONSHOT_API_KEY");
        assert!(!kimi.supports_custom_temperature());
        assert!(qwen.supports_custom_temperature());

        assert_eq!(Model::supported(), &["openai", "deepseek", "qwen", "kimi"]);
    }

    #[test]
    fn login_and_logout_round_trip_without_exposing_storage_details() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::at(directory.path().join("credentials.json"));

        store.login(Model::DeepSeek, "secret-key", None).unwrap();
        assert_eq!(store.active_model().unwrap(), Some(Model::DeepSeek));
        assert_eq!(
            store.api_key(Model::DeepSeek).unwrap().as_deref(),
            Some("secret-key")
        );

        store
            .login(
                Model::DeepSeek,
                "updated-key",
                Some("https://gateway.example/v1"),
            )
            .unwrap();
        assert_eq!(
            store.base_url(Model::DeepSeek).unwrap().as_deref(),
            Some("https://gateway.example/v1")
        );

        assert!(store.logout(Model::DeepSeek).unwrap());
        assert_eq!(store.api_key(Model::DeepSeek).unwrap(), None);
        assert_eq!(store.active_model().unwrap(), None);
    }

    #[test]
    fn obsolete_credential_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        std::fs::write(
            &path,
            r#"{
                "active_provider": "qwen",
                "providers": {"qwen": {"api_key": "old-key"}}
            }"#,
        )
        .unwrap();
        let store = CredentialStore::at(path);

        let error = store.active_model().unwrap_err().to_string();
        assert!(error.contains("decode credentials"));
    }

    #[test]
    fn runtime_config_uses_active_login_and_allows_explicit_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::at(directory.path().join("credentials.json"));
        store.login(Model::DeepSeek, "stored-key", None).unwrap();

        let configured = RuntimeModelConfig::resolve(ModelOverrides::default(), &store).unwrap();
        assert_eq!(configured.model, Model::DeepSeek);
        assert_eq!(configured.api_key, "stored-key");
        assert_eq!(configured.base_url, "https://api.deepseek.com");
        assert_eq!(configured.model_id, "deepseek-v4-flash");

        let overridden = RuntimeModelConfig::resolve(
            ModelOverrides {
                model: Some(Model::DeepSeek),
                api_key: Some("override-key".to_string()),
                base_url: Some("https://gateway.example/v1".to_string()),
                model_id: Some("custom-model".to_string()),
            },
            &store,
        )
        .unwrap();
        assert_eq!(overridden.api_key, "override-key");
        assert_eq!(overridden.base_url, "https://gateway.example/v1");
        assert_eq!(overridden.model_id, "custom-model");
    }

    #[test]
    fn runtime_config_allows_startup_without_a_saved_credential() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::at(directory.path().join("credentials.json"));

        let configured = RuntimeModelConfig::resolve(ModelOverrides::default(), &store).unwrap();

        assert_eq!(configured.model, Model::OpenAi);
        assert!(configured.api_key.is_empty());
    }

    #[test]
    fn runtime_config_uses_provider_specific_endpoint_and_model_from_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        std::fs::write(
            &path,
            r#"{
                "active_model": "deepseek",
                "models": {
                    "deepseek": {
                        "api_key": "stored-key",
                        "base_url": "https://gateway.example/v1",
                        "model_id": "deepseek-custom"
                    }
                }
            }"#,
        )
        .unwrap();
        let store = CredentialStore::at(path);

        let configured = RuntimeModelConfig::resolve(ModelOverrides::default(), &store).unwrap();
        assert_eq!(configured.api_key, "stored-key");
        assert_eq!(configured.base_url, "https://gateway.example/v1");
        assert_eq!(configured.model_id, "deepseek-custom");
    }

    #[test]
    fn discovered_credentials_live_under_the_user_home_directory() {
        let store = CredentialStore::discover().unwrap();
        let expected = dirs::home_dir().unwrap().join("noya/credentials.json");

        assert_eq!(store.path(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_private_to_the_current_user() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let store = CredentialStore::at(&path);
        store.login(Model::DeepSeek, "secret-key", None).unwrap();

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
