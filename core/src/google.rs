//! The Google connector: any number of Google accounts, and authorized calls to Google's APIs.
//!
//! Each account signs in once in the browser (OAuth 2.0 for desktop apps, with PKCE and a
//! redirect to a port on this machine). Its refresh token goes into the Keychain. The list of
//! accounts, which holds no secret, is a JSON file in the data directory.
//!
//! The OAuth client is the user's own, made in their Google Cloud project. Its id and secret are
//! in the Keychain too, never in this repository.
//!
//! Requests go through `crate::http`, so no token ever shows in the list of processes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::credentials::{self, Password, SignIns as _};
use crate::http::{self, curl, form_encoded, quoted};

/// The Keychain item that holds the OAuth client: the account is its id, the password its secret.
pub const CLIENT_SERVICE: &str = "dev.maverick.google-client";
/// The Keychain items that hold each account's refresh token, one per email address.
pub const TOKEN_SERVICE: &str = "dev.maverick.google";

const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const REVOKE: &str = "https://oauth2.googleapis.com/revoke";

pub const SHEETS: &str = "https://www.googleapis.com/auth/spreadsheets";
pub const DRIVE_READ: &str = "https://www.googleapis.com/auth/drive.readonly";
pub const GMAIL_READ: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const CALENDAR_READ: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// What a new account is asked for. Sign-in identity, then each service Maverick uses.
pub const SCOPES: &[&str] = &["openid", "email", SHEETS, DRIVE_READ, GMAIL_READ, CALENDAR_READ];

/// How long the browser sign-in may take before Maverick stops waiting.
const SIGN_IN_WAIT: Duration = Duration::from_secs(5 * 60);

/// The user's OAuth client.
#[derive(Clone, Debug)]
pub struct Client {
    pub id: String,
    secret: Password,
}

impl Client {
    pub fn new(id: impl Into<String>, secret: Password) -> Self {
        Self { id: id.into(), secret }
    }

    /// The client stored in the Keychain, or `None` before the user stores one.
    pub async fn stored() -> Result<Option<Self>, String> {
        Ok(credentials::Keychain
            .sign_in(CLIENT_SERVICE)
            .await?
            .map(|sign_in| Self::new(sign_in.account, sign_in.password)))
    }

    pub async fn save(&self) -> Result<(), String> {
        // One client at a time: a new id replaces the old item instead of sitting beside it.
        if let Some(old) = Self::stored().await?
            && old.id != self.id
        {
            credentials::forget(CLIENT_SERVICE, &old.id).await?;
        }
        credentials::store(CLIENT_SERVICE, &self.id, &self.secret).await
    }
}

/// A connected account. The refresh token is in the Keychain under its email address.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Account {
    pub email: String,
    pub scopes: Vec<String>,
    pub connected_at: String,
}

/// The accounts file in the data directory.
pub fn accounts_path() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("google-accounts.json"))
}

/// The connected accounts, in the order they were added. A missing file means none.
pub fn accounts(file: &Path) -> Result<Vec<Account>, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| format!("{} did not parse: {error}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("could not read {}: {error}", file.display())),
    }
}

fn save_accounts(file: &Path, accounts: &[Account]) -> Result<(), String> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(|error| format!("could not create {}: {error}", folder.display()))?;
    }
    let text = serde_json::to_string_pretty(accounts).expect("accounts always serialize");
    std::fs::write(file, text + "\n").map_err(|error| format!("could not write {}: {error}", file.display()))
}

/// Adds `account` to the file, or replaces the entry with the same email.
fn remember(file: &Path, account: Account) -> Result<(), String> {
    let mut all = accounts(file)?;
    match all.iter_mut().find(|known| known.email == account.email) {
        Some(known) => *known = account,
        None => all.push(account),
    }
    save_accounts(file, &all)
}

/// Signs one account in through the browser and stores it. `open` shows the sign-in page.
/// Signing in an account that is already connected refreshes its token and scopes.
pub async fn connect(client: &Client, file: &Path, open: impl FnOnce(&str)) -> Result<Account, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("could not listen for the sign-in: {error}"))?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_text(64)?;
    let state = random_text(24)?;
    open(&authorize_url(&client.id, &redirect, &challenge(&verifier), &state));

    let code = tokio::time::timeout(SIGN_IN_WAIT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| "the sign-in was not finished within 5 minutes".to_owned())??;
    let reply = post_form(
        TOKEN,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("client_id", &client.id),
            ("client_secret", client.secret.reveal()),
        ],
    )
    .await?;
    let email = email_in(&reply)?;
    let refresh = reply["refresh_token"]
        .as_str()
        .ok_or("Google gave no refresh token; remove Maverick's access in the account's settings and connect again")?;
    credentials::store(TOKEN_SERVICE, &email, &Password::new(refresh)).await?;
    let account = Account {
        email,
        scopes: reply["scope"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        connected_at: Timestamp::now().to_string(),
    };
    remember(file, account.clone())?;
    Ok(account)
}

/// Removes an account: Google forgets the grant, then the Keychain and the file forget the account.
pub async fn disconnect(file: &Path, email: &str) -> Result<(), String> {
    if let Some(refresh) = credentials::secret(TOKEN_SERVICE, email).await? {
        // A token Google already dropped answers with an error. The local copy goes either way.
        let _ = post_form(REVOKE, &[("token", refresh.reveal())]).await;
    }
    credentials::forget(TOKEN_SERVICE, email).await?;
    let mut all = accounts(file)?;
    all.retain(|account| account.email != email);
    save_accounts(file, &all)
}

/// A short-lived access token for `email`, from its refresh token.
pub async fn access_token(client: &Client, email: &str) -> Result<Password, String> {
    let refresh = credentials::secret(TOKEN_SERVICE, email)
        .await?
        .ok_or_else(|| format!("{email} is not connected"))?;
    let reply = post_form(
        TOKEN,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.reveal()),
            ("client_id", &client.id),
            ("client_secret", client.secret.reveal()),
        ],
    )
    .await
    .map_err(|error| format!("{email} could not be signed in again ({error}); connect it again"))?;
    reply["access_token"]
        .as_str()
        .map(Password::new)
        .ok_or_else(|| "Google's token reply had no access token".to_owned())
}

/// One authorized call to a Google API.
pub async fn call(token: &Password, method: &str, url: &str, body: Option<&Value>) -> Result<Value, String> {
    http::call("Google", token, method, url, body).await
}

async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let body: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{name}={}", form_encoded(value)))
        .collect();
    let config = format!(
        "url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/x-www-form-urlencoded\"\ndata-binary = {}\n",
        quoted(url),
        quoted(&body.join("&"))
    );
    let (status, reply) = curl(&config).await?;
    let json: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
    if status != 200 {
        let why = json["error_description"]
            .as_str()
            .or(json["error"].as_str())
            .unwrap_or("no reason given");
        return Err(format!("Google answered with status {status}: {why}"));
    }
    Ok(json)
}

fn authorize_url(client_id: &str, redirect: &str, challenge: &str, state: &str) -> String {
    let scopes = SCOPES.join(" ");
    let fields = [
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", &scopes),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        // A refresh token, and the consent page every time, so Google always sends one.
        ("access_type", "offline"),
        ("prompt", "consent select_account"),
        ("include_granted_scopes", "true"),
    ];
    let query: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{name}={}", form_encoded(value)))
        .collect();
    format!("{AUTHORIZE}?{}", query.join("&"))
}

/// The PKCE challenge for `verifier`.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random text from the URL-safe alphabet, from the system's random source.
fn random_text(length: usize) -> Result<String, String> {
    use std::io::Read as _;
    let mut bytes = vec![0u8; length];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| format!("could not read random bytes: {error}"))?;
    let mut text = URL_SAFE_NO_PAD.encode(&bytes);
    text.truncate(length);
    Ok(text)
}

/// Answers browser requests on `listener` until one carries the sign-in's code.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| format!("the sign-in redirect failed: {error}"))?;
        let mut request = Vec::new();
        let mut chunk = [0u8; 2048];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 16 * 1024 {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let first_line = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or("")
            .to_owned();
        let outcome = redirect_outcome(&first_line, state);
        let page = match &outcome {
            Some(Ok(_)) => "Maverick is connected. You can close this tab.",
            Some(Err(_)) => "Maverick could not connect. Go back to Maverick to see why.",
            // Something else, such as the browser asking for an icon.
            None => "",
        };
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

/// What a redirect request says: the code, Google's refusal, or `None` for an unrelated request.
fn redirect_outcome(first_line: &str, state: &str) -> Option<Result<String, String>> {
    let target = first_line.strip_prefix("GET ")?.split(' ').next()?;
    let query = target.strip_prefix("/?")?;
    let field = |name: &str| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| crate::jobs::percent_decoded(&value.replace('+', " ")))
        })
    };
    if field("state").as_deref() != Some(state) {
        return Some(Err("the sign-in reply did not match the request; try again".to_owned()));
    }
    if let Some(error) = field("error") {
        return Some(Err(format!("Google refused the sign-in: {error}")));
    }
    Some(field("code").ok_or_else(|| "the sign-in reply had no code".to_owned()))
}

/// The email address in the token reply's ID token. It came straight from Google over TLS,
/// so its signature is not checked again here.
fn email_in(reply: &Value) -> Result<String, String> {
    let token = reply["id_token"].as_str().ok_or("Google's reply had no ID token")?;
    let payload = token.split('.').nth(1).ok_or("the ID token is malformed")?;
    let claims: Value = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or("the ID token is malformed")?;
    claims["email"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the ID token has no email address".to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_sign_in_address_asks_for_offline_access_with_pkce() {
        let url = authorize_url("123.apps.googleusercontent.com", "http://127.0.0.1:5000", "abc", "xyz");
        assert!(
            url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=123.apps.googleusercontent.com&")
        );
        assert!(url.contains("&redirect_uri=http%3A%2F%2F127.0.0.1%3A5000&"));
        assert!(url.contains("&code_challenge=abc&code_challenge_method=S256&state=xyz&access_type=offline&"));
        assert!(url.contains("scope=openid%20email%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fspreadsheets%20"));
    }

    #[test]
    fn the_challenge_is_the_rfc_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let text = random_text(64).unwrap();
        assert_eq!(text.len(), 64);
        assert_ne!(text, random_text(64).unwrap());
    }

    #[test]
    fn a_redirect_gives_the_code_only_when_the_state_matches() {
        let line = "GET /?state=s1&code=4%2F0Ab-x_y&scope=email HTTP/1.1";
        assert_eq!(redirect_outcome(line, "s1"), Some(Ok("4/0Ab-x_y".to_owned())));
        assert!(matches!(redirect_outcome(line, "other"), Some(Err(_))));
        let refused = "GET /?state=s1&error=access_denied HTTP/1.1";
        assert_eq!(
            redirect_outcome(refused, "s1"),
            Some(Err("Google refused the sign-in: access_denied".to_owned()))
        );
        assert_eq!(redirect_outcome("GET /favicon.ico HTTP/1.1", "s1"), None);
    }

    #[test]
    fn the_email_comes_from_the_id_token() {
        let claims = URL_SAFE_NO_PAD.encode(json!({ "email": "ada@example.com", "sub": "1" }).to_string());
        let reply = json!({ "id_token": format!("header.{claims}.signature") });
        assert_eq!(email_in(&reply), Ok("ada@example.com".to_owned()));
        assert!(email_in(&json!({ "id_token": "bad" })).is_err());
    }

    #[test]
    fn accounts_are_added_once_and_replaced_by_email() {
        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("google-accounts.json");
        assert_eq!(accounts(&file), Ok(Vec::new()));
        let account = |email: &str, scope: &str| Account {
            email: email.into(),
            scopes: vec![scope.into()],
            connected_at: "2026-11-22T17:00:00Z".into(),
        };
        remember(&file, account("ada@example.com", "email")).unwrap();
        remember(&file, account("bo@example.com", "email")).unwrap();
        remember(&file, account("ada@example.com", SHEETS)).unwrap();
        assert_eq!(
            accounts(&file).unwrap(),
            [account("ada@example.com", SHEETS), account("bo@example.com", "email")]
        );
    }
}
