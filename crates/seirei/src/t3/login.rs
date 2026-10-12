use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use seirei_proto::model::Access;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// A loopback redirect: T3 hands the code back in the decision's answer, so nothing listens here.
const REDIRECT: &str = "http://127.0.0.1/callback";

#[derive(Serialize, Deserialize)]
pub struct Credentials {
    pub origin: String,
    pub token: String,
    pub access: Access,
    pub expires_at: SystemTime,
}

pub enum PairingCode {
    Mint,
    Given(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("could not create a pairing code with `{} auth pairing create`: {detail}", .cli.display())]
    Pairing { cli: PathBuf, detail: String },
    #[error("T3 refused the login: {0}")]
    Refused(String),
    #[error("T3 is unreachable at {origin}: {detail}")]
    Unreachable { origin: String, detail: String },
    #[error("saving credentials: {0}")]
    Save(#[from] std::io::Error),
}

pub async fn login(origin: &str, cli: &Path, access: Access, code: PairingCode) -> Result<Credentials, LoginError> {
    let code = match code {
        PairingCode::Mint => pairing_code(cli).await?,
        PairingCode::Given(code) => code,
    };
    let http = reqwest::Client::new();
    let client = json!({ "client_name": "seirei", "redirect_uris": [REDIRECT] });
    let registered: Registered = send(origin, http.post(format!("{origin}/oauth/mcp/register")).json(&client)).await?;
    let pkce = Pkce::fresh();
    let state = random_token(16);
    let decision = json!({
        "authorization": {
            "response_type": "code",
            "client_id": registered.client_id,
            "redirect_uri": REDIRECT,
            "code_challenge": pkce.challenge,
            "code_challenge_method": "S256",
            "state": state,
        },
        "decision": { "_tag": "pairing-code", "access": access, "code": code },
    });
    let decided: Decided = send(origin, http.post(format!("{origin}/oauth/mcp/decision")).json(&decision)).await?;
    let authorization = authorization_code(&decided.redirect_to, &state)?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", &authorization),
        ("redirect_uri", REDIRECT),
        ("client_id", &registered.client_id),
        ("code_verifier", &pkce.verifier),
    ];
    let issued: Issued = send(origin, http.post(format!("{origin}/oauth/mcp/token")).form(&form)).await?;
    let expires_at = SystemTime::now() + Duration::from_secs(issued.expires_in);
    Ok(Credentials { origin: origin.into(), token: issued.access_token, access, expires_at })
}

#[derive(Deserialize)]
struct Registered {
    client_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Decided {
    redirect_to: String,
}

#[derive(Deserialize)]
struct Issued {
    access_token: String,
    expires_in: u64,
}

async fn pairing_code(cli: &Path) -> Result<String, LoginError> {
    #[derive(Deserialize)]
    struct Pairing {
        credential: String,
    }
    let failed = |detail: String| LoginError::Pairing { cli: cli.to_owned(), detail };
    let output = (tokio::process::Command::new(cli))
        .args(["auth", "pairing", "create", "--json", "--label", "seirei"])
        .output()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !output.status.success() {
        return Err(failed(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    let pairing: Pairing = serde_json::from_slice(&output.stdout).map_err(|e| failed(e.to_string()))?;
    Ok(pairing.credential)
}

async fn send<T: DeserializeOwned>(origin: &str, request: reqwest::RequestBuilder) -> Result<T, LoginError> {
    let unreachable = |e: reqwest::Error| LoginError::Unreachable { origin: origin.into(), detail: e.to_string() };
    let response = request.send().await.map_err(unreachable)?;
    let status = response.status();
    let body = response.bytes().await.map_err(unreachable)?;
    if !status.is_success() {
        let parsed: Option<Value> = serde_json::from_slice(&body).ok();
        let message = parsed.as_ref().and_then(|v| v.get("message")).and_then(Value::as_str).map(String::from);
        let message = message.unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        return Err(LoginError::Refused(format!("HTTP {status}: {message}")));
    }
    serde_json::from_slice(&body).map_err(|e| LoginError::Refused(format!("unexpected answer from T3: {e}")))
}

struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn fresh() -> Self {
        let verifier = random_token(48);
        Self { challenge: challenge(&verifier), verifier }
    }
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0; bytes];
    getrandom::fill(&mut buf).expect("the OS random source is available");
    URL_SAFE_NO_PAD.encode(buf)
}

/// A `redirectTo` that carries `error=` instead is a refusal even though T3 answered 200.
fn authorization_code(redirect_to: &str, state: &str) -> Result<String, LoginError> {
    let url = reqwest::Url::parse(redirect_to)
        .map_err(|e| LoginError::Refused(format!("T3 redirected to {redirect_to:?}: {e}")))?;
    let param = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
    if let Some(error) = param("error") {
        let description = param("error_description").map(|d| format!(": {d}")).unwrap_or_default();
        return Err(LoginError::Refused(format!("{error}{description}")));
    }
    if param("state").as_deref() != Some(state) {
        return Err(LoginError::Refused("the authorization answered another login (state mismatch)".into()));
    }
    param("code").ok_or_else(|| LoginError::Refused("T3 redirected without an authorization code".into()))
}

pub fn save(path: &Path, credentials: &Credentials) -> Result<(), LoginError> {
    let json = serde_json::to_vec_pretty(credentials).expect("credentials serialize infallibly");
    Ok(crate::write_private(path, &json)?)
}

pub fn load(path: &Path) -> io::Result<Option<Credentials>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pkce_matches_rfc_7636() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "the RFC 7636 appendix B vector"
        );
        let pkce = Pkce::fresh();
        assert_eq!(pkce.verifier.len(), 64);
        assert!(pkce.verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)));
        assert_eq!(pkce.challenge, challenge(&pkce.verifier));
        assert_ne!(Pkce::fresh().verifier, pkce.verifier, "every login gets its own verifier");
    }

    #[test]
    fn the_authorization_code_comes_from_a_redirect_with_our_state() {
        let ok = "http://127.0.0.1/callback?code=abc%2Fdef&state=s1&iss=http%3A%2F%2F127.0.0.1%3A3773";
        assert_eq!(authorization_code(ok, "s1").expect("a code"), "abc/def");
        let refused = "http://127.0.0.1/callback?error=access_denied&error_description=pairing+code+expired&state=s1";
        assert!(
            matches!(authorization_code(refused, "s1"), Err(LoginError::Refused(why)) if why == "access_denied: pairing code expired")
        );
        assert!(matches!(authorization_code(ok, "s2"), Err(LoginError::Refused(_))), "a foreign state is refused");
    }

    #[test]
    fn credentials_round_trip_privately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested/credentials.json");
        assert!(load(&path).expect("a missing file is fine").is_none());
        let saved = Credentials {
            origin: "http://127.0.0.1:3773".into(),
            token: "t3-secret".into(),
            access: Access::FullAccess,
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_820_528),
        };
        save(&path, &saved).expect("saved");
        let mode = std::fs::metadata(&path).expect("written").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the owner can read the token");
        let loaded = load(&path).expect("readable").expect("present");
        assert_eq!((loaded.origin, loaded.token, loaded.access), (saved.origin, saved.token, saved.access));
        assert_eq!(loaded.expires_at, saved.expires_at);
    }
}
