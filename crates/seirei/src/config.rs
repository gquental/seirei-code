use crate::t3::{T3Error, login};
use anyhow::{Context, bail};
use seirei_proto::{
    ids::{MachineId, ModelId, ProviderInstanceId},
    model::{Access, ModelSelection},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::SystemTime,
};

const MACHINES_HEADER: &str = "# The T3 servers seirei reaches besides this machine's own.\n\
                               # Seirei rewrites this file when you add or remove a machine.\n\n";

pub struct Config {
    /// The root, holding what a user may edit: `config.toml`, `machines.toml`, and `credentials/`.
    pub dir: PathBuf,
    /// `state/` in the root, which only seirei writes.
    pub state_dir: PathBuf,
    /// `local` first, then by name.
    pub machines: Vec<MachineConfig>,
}

pub struct MachineConfig {
    pub id: MachineId,
    pub endpoint: Endpoint,
    pub token: Option<Token>,
    pub coordinator_model: Option<ModelSelection>,
}

#[derive(Clone, Debug)]
pub enum Endpoint {
    /// The origin is re-read from `server-runtime.json` on every connect, because T3 may restart
    /// on another port.
    Local {
        t3_home: PathBuf,
    },
    Tailnet(TailnetOrigin),
}

impl Endpoint {
    pub fn origin(&self) -> Result<String, T3Error> {
        match self {
            Self::Local { t3_home } => crate::t3::origin(t3_home),
            Self::Tailnet(origin) => Ok(origin.as_str().into()),
        }
    }

    /// The origin when it can be read, else where seirei looks for it.
    pub fn address(&self) -> String {
        match self {
            Self::Local { t3_home } => self.origin().unwrap_or_else(|_| t3_home.display().to_string()),
            Self::Tailnet(origin) => origin.as_str().into(),
        }
    }
}

/// The `https` origin of a T3 served on a tailnet, built only by `parse`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TailnetOrigin(String);

impl TailnetOrigin {
    /// Accepts a MagicDNS name under `.ts.net` or a Tailscale IP, with an optional port and an
    /// optional `/mcp`, and keeps only the origin.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let refuse = |why: String| {
            anyhow::anyhow!(
                "{raw:?} {why}. For now seirei reaches other machines only over Tailscale: give the T3 address as \
                 https://<machine>.<tailnet>.ts.net, with its port when it is not 443, or as https:// and a Tailscale IP"
            )
        };
        let url = reqwest::Url::parse(raw.trim()).map_err(|e| refuse(format!("is not an address ({e})")))?;
        if url.scheme() != "https" {
            return Err(refuse(format!("uses {}, not https", url.scheme())));
        }
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err(refuse("carries more than an address".into()));
        }
        if !matches!(url.path(), "/" | "/mcp" | "/mcp/") {
            return Err(refuse(format!("has the path {}, and T3 serves MCP at /mcp", url.path())));
        }
        let host = url.host_str().unwrap_or_default();
        let v6 = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).map(str::parse::<Ipv6Addr>);
        let tailnet = match (host.parse::<Ipv4Addr>(), v6) {
            (Ok(ip), _) => ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]),
            (_, Some(Ok(ip))) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
            _ => host.ends_with(".ts.net"),
        };
        if !tailnet {
            return Err(refuse("is not a Tailscale name or IP".into()));
        }
        Ok(Self(url.origin().ascii_serialization()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TailnetOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
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

/// One table of `machines.toml`, kept as written so a rewrite changes only what it means to.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct MachineEntry {
    address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordinator_model: Option<String>,
}

type MachinesFile = BTreeMap<MachineId, MachineEntry>;

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(seirei_proto::home::root()?)
    }

    fn load_from(dir: PathBuf) -> anyhow::Result<Self> {
        let path = dir.join("config.toml");
        let file: File = read_toml(&path)?;
        let coordinator_model = (file.coordinator_model.as_deref().map(model_selection).transpose())
            .with_context(|| format!("parsing {}", path.display()))?;
        let home = std::env::home_dir().context("no home directory")?;
        let t3_home = (env("T3CODE_HOME").map(PathBuf::from)).or(file.t3_home).unwrap_or_else(|| home.join(".t3"));
        let state_dir = dir.join("state");
        let endpoint = Endpoint::Local { t3_home };
        let token = match env("SEIREI_T3_TOKEN").or(file.t3_token) {
            Some(token) => Some(Token { secret: Secret(token), access: None }),
            None => saved_token(&credentials(&dir, &MachineId::local()), &endpoint)?,
        };
        let mut machines = vec![MachineConfig { id: MachineId::local(), endpoint, token, coordinator_model }];
        machines.extend(remote_machines(&dir)?);
        Ok(Self { dir, state_dir, machines })
    }

    pub fn machine(&self, id: &MachineId) -> anyhow::Result<&MachineConfig> {
        (self.machines.iter().find(|machine| &machine.id == id)).with_context(|| {
            let known: Vec<&str> = self.machines.iter().map(|machine| machine.id.as_str()).collect();
            format!("no machine is named {id}; seirei knows {}", known.join(", "))
        })
    }
}

/// Writes `id` at `origin` into `machines.toml` unless it is there already, and returns the
/// machine as seirei now reaches it, with any credential saved for that origin.
pub fn add_machine(dir: &Path, id: &MachineId, origin: &TailnetOrigin) -> anyhow::Result<MachineConfig> {
    if id.is_local() {
        bail!("{id} is this machine's own T3; seirei always has it");
    }
    let mut file = machines_file(dir)?;
    match file.get(id) {
        Some(held) if TailnetOrigin::parse(&held.address).ok().as_ref() == Some(origin) => {}
        Some(held) => bail!("{id} is already at {}; remove it first to move it", held.address),
        None => {
            file.insert(id.clone(), MachineEntry { address: origin.to_string(), coordinator_model: None });
            write_machines(dir, &file)?;
        }
    }
    remote_machine(dir, id, &file[id])
}

/// Whether `machines.toml` named `id`.
pub fn remove_machine(dir: &Path, id: &MachineId) -> anyhow::Result<bool> {
    if id.is_local() {
        bail!("{id} is this machine's own T3; seirei always has it");
    }
    let mut file = machines_file(dir)?;
    let held = file.remove(id).is_some();
    if held {
        write_machines(dir, &file)?;
    }
    Ok(held)
}

fn write_machines(dir: &Path, file: &MachinesFile) -> anyhow::Result<()> {
    let path = dir.join("machines.toml");
    let text = format!("{MACHINES_HEADER}{}", toml::to_string(file).context("writing machines.toml")?);
    crate::write_private(&path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

fn remote_machines(dir: &Path) -> anyhow::Result<Vec<MachineConfig>> {
    machines_file(dir)?.iter().map(|(id, entry)| remote_machine(dir, id, entry)).collect()
}

fn remote_machine(dir: &Path, id: &MachineId, entry: &MachineEntry) -> anyhow::Result<MachineConfig> {
    let parsed = (TailnetOrigin::parse(&entry.address).map(Endpoint::Tailnet))
        .and_then(|endpoint| Ok((endpoint, entry.coordinator_model.as_deref().map(model_selection).transpose()?)));
    let (endpoint, coordinator_model) =
        parsed.with_context(|| format!("parsing [{id}] in {}", dir.join("machines.toml").display()))?;
    let token = saved_token(&credentials(dir, id), &endpoint)?;
    Ok(MachineConfig { id: id.clone(), endpoint, token, coordinator_model })
}

fn machines_file(dir: &Path) -> anyhow::Result<MachinesFile> {
    let path = dir.join("machines.toml");
    let file: MachinesFile = read_toml(&path)?;
    if file.contains_key(&MachineId::local()) {
        bail!("{} names a machine local; that name is this machine's own T3", path.display());
    }
    Ok(file)
}

pub fn credentials(dir: &Path, machine: &MachineId) -> PathBuf {
    dir.join("credentials").join(format!("{machine}.json"))
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

fn read_toml<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A credential saved for another origin, or one that lapsed, is not used.
fn saved_token(path: &Path, endpoint: &Endpoint) -> anyhow::Result<Option<Token>> {
    let Some(saved) = login::load(path).with_context(|| format!("reading {}", path.display()))? else {
        return Ok(None);
    };
    let fresh = saved.expires_at > SystemTime::now();
    let here = endpoint.origin().map_or(true, |origin| origin == saved.origin);
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

    fn save(path: &Path, origin: &str, expires_at: SystemTime) {
        let credentials =
            login::Credentials { origin: origin.into(), token: "t3-secret".into(), access: Access::Auto, expires_at };
        login::save(path, &credentials).expect("saved");
    }

    #[test]
    fn the_saved_credential_carries_its_grant_until_it_lapses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("credentials/local.json");
        let local = Endpoint::Local { t3_home: dir.path().join("t3") };
        save(&path, "http://127.0.0.1:3773", SystemTime::now() + Duration::from_secs(3600));
        let token = saved_token(&path, &local).expect("readable").expect("fresh");
        assert_eq!((token.secret, token.access), (Secret("t3-secret".into()), Some(Access::Auto)));
        save(&path, "http://127.0.0.1:3773", SystemTime::now() - Duration::from_secs(1));
        assert!(saved_token(&path, &local).expect("readable").is_none(), "a lapsed credential is not used");
    }

    #[test]
    fn config_is_read_from_the_root_and_state_kept_under_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join(".seirei/code");
        let studio = MachineId::parse("studio").unwrap();
        let at = TailnetOrigin::parse("https://mac-studio.tail123b88.ts.net").unwrap();
        add_machine(&root, &studio, &at).expect("added");
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the root seirei creates is private");
        save(&credentials(&root, &studio), at.as_str(), SystemTime::now() + Duration::from_secs(3600));
        std::fs::write(root.join("config.toml"), "coordinator_model = \"claudeAgent/claude-opus-5-5\"\n").unwrap();

        let config = Config::load_from(root.clone()).expect("loads");
        assert_eq!((&config.dir, &config.state_dir), (&root, &root.join("state")));
        let [local, remote] = &config.machines[..] else { panic!("local and studio") };
        let model = local.coordinator_model.as_ref().map(|model| model.model.as_str());
        assert_eq!(model, Some("claude-opus-5-5"), "config.toml is read from the root");
        assert!(remote.id == studio && remote.token.is_some(), "so are machines.toml and the credential");
    }

    #[test]
    fn a_tailscale_address_is_kept_as_its_https_origin() {
        let origin = |raw: &str| TailnetOrigin::parse(raw).map(|origin| origin.to_string()).map_err(|e| e.to_string());
        let macbook = "https://macbook-pro-de-gui.tail123b88.ts.net:4043";
        assert_eq!(origin("https://macbook-pro-de-gui.tail123b88.ts.net:4043/").as_deref(), Ok(macbook));
        assert_eq!(origin("https://macbook-pro-de-gui.tail123b88.ts.net:4043/mcp").as_deref(), Ok(macbook));
        assert_eq!(
            origin(" HTTPS://Mac-Studio.tail123b88.ts.net:443 ").as_deref(),
            Ok("https://mac-studio.tail123b88.ts.net")
        );
        assert_eq!(origin("https://100.120.250.92:4043").as_deref(), Ok("https://100.120.250.92:4043"));
        assert_eq!(origin("https://[fd7a:115c:a1e0::5d01]/").as_deref(), Ok("https://[fd7a:115c:a1e0::5d01]"));
        for (raw, why) in [
            ("http://mac-studio.tail123b88.ts.net", "uses http, not https"),
            ("mac-studio.tail123b88.ts.net", "is not an address"),
            ("https://example.com", "is not a Tailscale name or IP"),
            ("https://ts.net", "is not a Tailscale name or IP"),
            ("https://100.128.0.1", "is not a Tailscale name or IP"),
            ("https://192.168.1.20:4043", "is not a Tailscale name or IP"),
            ("https://[fd7a:115c:a1e1::1]", "is not a Tailscale name or IP"),
            ("https://mac-studio.tail123b88.ts.net/api", "has the path /api"),
            ("https://gui:secret@mac-studio.tail123b88.ts.net", "carries more than an address"),
            ("https://mac-studio.tail123b88.ts.net/mcp?token=x", "carries more than an address"),
        ] {
            let refused = origin(raw).expect_err(raw);
            assert!(refused.contains(why), "{raw}: {refused}");
            assert!(refused.contains("only over Tailscale"), "{raw}: {refused}");
        }
    }

    #[test]
    fn machines_are_added_and_removed_in_machines_toml_and_find_their_credentials() {
        let dir = tempfile::tempdir().expect("temp dir");
        let macbook = MachineId::parse("macbook").unwrap();
        let at = TailnetOrigin::parse("https://macbook-pro-de-gui.tail123b88.ts.net:4043/").unwrap();
        let added = add_machine(dir.path(), &macbook, &at).expect("added");
        assert!((added.endpoint.address(), added.token.is_none()) == (at.to_string(), true), "not paired yet");
        let path = dir.path().join("machines.toml");
        let written = std::fs::read_to_string(&path).unwrap();
        let elsewhere = TailnetOrigin::parse("https://macbook-pro-de-gui.tail123b88.ts.net").unwrap();
        assert!(add_machine(dir.path(), &macbook, &elsewhere).is_err(), "a name keeps its address until removed");
        assert!(add_machine(dir.path(), &MachineId::local(), &at).is_err(), "local is implicit");

        let edited = written + "coordinator_model = \"codex/gpt-6.1-sol\"\n";
        std::fs::write(&path, &edited).unwrap();
        save(&credentials(dir.path(), &macbook), at.as_str(), SystemTime::now() + Duration::from_secs(3600));
        let again = add_machine(dir.path(), &macbook, &at).expect("adding again changes nothing");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited, "the file is left as it was");
        let model = again.coordinator_model.as_ref().map(|model| model.model.as_str());
        assert_eq!((again.token.is_some(), model), (true, Some("gpt-6.1-sol")), "it is read back as written");
        let studio = MachineId::parse("studio").unwrap();
        add_machine(dir.path(), &studio, &TailnetOrigin::parse("https://mac-studio.tail123b88.ts.net").unwrap())
            .unwrap();
        save(
            &credentials(dir.path(), &studio),
            "https://elsewhere.tail123b88.ts.net",
            SystemTime::now() + Duration::from_secs(3600),
        );

        let machines = remote_machines(dir.path()).expect("the file parses");
        let shown: Vec<(&str, String, bool, Option<&str>)> = (machines.iter())
            .map(|m| {
                let model = m.coordinator_model.as_ref().map(|model| model.model.as_str());
                (m.id.as_str(), m.endpoint.address(), m.token.is_some(), model)
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("macbook", at.to_string(), true, Some("gpt-6.1-sol")),
                ("studio", "https://mac-studio.tail123b88.ts.net".into(), false, None),
            ],
            "a rewrite keeps a hand-added key, and a credential for another origin is not used"
        );

        assert!(remove_machine(dir.path(), &macbook).expect("removed"));
        assert!(!remove_machine(dir.path(), &macbook).expect("nothing to remove"), "the file no longer names it");
        assert!(remove_machine(dir.path(), &MachineId::local()).is_err(), "local is implicit");
        let left: Vec<String> = remote_machines(dir.path()).unwrap().iter().map(|m| m.id.to_string()).collect();
        assert_eq!(left, ["studio"]);

        std::fs::write(&path, "[local]\naddress = \"https://mac-studio.tail123b88.ts.net\"\n").unwrap();
        assert!(remote_machines(dir.path()).is_err(), "local cannot be configured");
        std::fs::write(&path, "[box]\naddress = \"http://192.168.1.2:3773\"\n").unwrap();
        let refused = remote_machines(dir.path()).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(refused.contains("[box]") && refused.contains("only over Tailscale"), "{refused}");
    }
}
