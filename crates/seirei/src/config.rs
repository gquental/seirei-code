use crate::t3::login;
use anyhow::Context;
use seirei_proto::{
    ids::{ModelId, ProviderInstanceId},
    model::{Access, ModelSelection},
};
use serde::Deserialize;
use std::{
    fmt,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub struct Config {
    pub t3_home: PathBuf,
    pub state_dir: PathBuf,
    pub credentials: PathBuf,
    pub token: Option<Token>,
    pub coordinator_model: Option<ModelSelection>,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub secret: Secret,
    pub access: Option<Access>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    t3_token: Option<String>,
    t3_home: Option<PathBuf>,
    coordinator_model: Option<String>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "seirei").context("no home directory")?;
        let path = dirs.config_dir().join("config.toml");
        let file = read_file(&path)?;
        let coordinator_model = (file.coordinator_model.as_deref().map(model_selection).transpose())
            .with_context(|| format!("parsing {}", path.display()))?;
        let home = directories::BaseDirs::new().context("no home directory")?.home_dir().to_path_buf();
        let t3_home = (env("T3CODE_HOME").map(PathBuf::from)).or(file.t3_home).unwrap_or_else(|| home.join(".t3"));
        let state_dir = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir()).to_path_buf();
        let credentials = dirs.config_dir().join("credentials.json");
        let token = match env("SEIREI_T3_TOKEN").or(file.t3_token) {
            Some(token) => Some(Token { secret: Secret(token), access: None }),
            None => saved_token(&credentials, &t3_home)?,
        };
        Ok(Self { t3_home, state_dir, credentials, token, coordinator_model })
    }
}

fn model_selection(raw: &str) -> anyhow::Result<ModelSelection> {
    match raw.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() && !raw.contains(char::is_whitespace) => {
            Ok(ModelSelection { provider: ProviderInstanceId::new(provider), model: ModelId::new(model) })
        }
        _ => anyhow::bail!(
            "coordinator_model is {raw:?}; write it as \"<providerInstanceId>/<model>\", like \"claudeAgent/claude-opus-5-5\""
        ),
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn read_file(path: &Path) -> anyhow::Result<File> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn saved_token(path: &Path, t3_home: &Path) -> anyhow::Result<Option<Token>> {
    let Some(saved) = login::load(path).with_context(|| format!("reading {}", path.display()))? else {
        return Ok(None);
    };
    let fresh = saved.expires_at > SystemTime::now();
    let here = crate::t3::origin(t3_home).map_or(true, |origin| origin == saved.origin);
    Ok((fresh && here).then_some(Token { secret: Secret(saved.token), access: Some(saved.access) }))
}

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn coordinator_model_is_a_provider_and_a_model() {
        let parsed = model_selection("codex/openrouter/gpt-6").expect("valid");
        assert_eq!((parsed.provider.as_str(), parsed.model.as_str()), ("codex", "openrouter/gpt-6"));
        for malformed in ["claude-opus-5-5", "/claude-opus-5-5", "claudeAgent/", "claudeAgent / claude-opus-5-5", ""] {
            assert!(model_selection(malformed).is_err(), "{malformed:?} is not <provider>/<model>");
        }
        let file: File = toml::from_str("coordinator_model = \"claudeAgent/claude-opus-5-5\"").expect("a known key");
        assert_eq!(file.coordinator_model.as_deref(), Some("claudeAgent/claude-opus-5-5"));
    }

    #[test]
    fn the_saved_credential_carries_its_grant_until_it_lapses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (path, t3_home) = (dir.path().join("credentials.json"), dir.path().join("t3"));
        let save = |expires_at| {
            let credentials = login::Credentials {
                origin: "http://127.0.0.1:3773".into(),
                token: "t3-secret".into(),
                access: Access::Auto,
                expires_at,
            };
            login::save(&path, &credentials).expect("saved");
        };
        save(SystemTime::now() + Duration::from_secs(3600));
        let token = saved_token(&path, &t3_home).expect("readable").expect("fresh");
        assert_eq!((token.secret, token.access), (Secret("t3-secret".into()), Some(Access::Auto)));
        save(SystemTime::now() - Duration::from_secs(1));
        assert!(saved_token(&path, &t3_home).expect("readable").is_none(), "a lapsed credential is not used");
    }
}
