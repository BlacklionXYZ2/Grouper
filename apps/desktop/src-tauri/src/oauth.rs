//! Desktop Google OAuth 2.0 sign-in with loopback redirect (`http://localhost:47600/oauth/google/return`).
//!
//! TODO(Q5: Desktop OAuth client — docs/local-first-design.md §7 Q5):
//!   1. Create a "Desktop app" OAuth 2.0 client in GCP project `grouper-515912` and set
//!      `DEFAULT_DESKTOP_CLIENT_ID` below (or supply `GROUPER_GOOGLE_DESKTOP_CLIENT_ID` via env /
//!      `google_desktop_client_id` in `%LOCALAPPDATA%\Grouper\desktop.json`).
//!   2. Add that Desktop client ID to `DEFAULT_CLIENT_IDS` in `crates/hub/src/google_auth.rs` so
//!      hubs trust ID tokens minted for the Windows desktop client without `GROUPER_GOOGLE_CLIENT_IDS`.
//!   3. Add `"windows"` alongside `"macos"` / `"cli"` in `POST /v1/auth/google` (`crates/hub/src/google_auth.rs`)
//!      for remote-hub sign-in (`claim_local_client` already accepts `"windows"`).
//!
//! Until Q5 is completed, local-hub sign-in works via `http://127.0.0.1:47600/#/token-signin` from loopback.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::settings::DesktopSettings;

pub const DESKTOP_CLIENT_ID_ENV: &str = "GROUPER_GOOGLE_DESKTOP_CLIENT_ID";
pub const DESKTOP_CLIENT_SECRET_ENV: &str = "GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET";
/// Filled in once the owner creates the Desktop OAuth client in Google Cloud Console (see TODO above).
pub const DEFAULT_DESKTOP_CLIENT_ID: &str = "";

pub const LOOPBACK_PORT: u16 = 47600;
pub const LOOPBACK_RETURN_PATH: &str = "/oauth/google/return";
pub const LOOPBACK_REDIRECT_URI: &str = "http://localhost:47600/oauth/google/return";

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_HTTP_REQ_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug)]
pub struct DesktopOAuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Resolve the Desktop Google OAuth client from env (`GROUPER_GOOGLE_DESKTOP_CLIENT_ID`), then
/// `desktop.json` (`google_desktop_client_id`), then `DEFAULT_DESKTOP_CLIENT_ID`.
pub fn resolve_oauth_config(settings: &DesktopSettings) -> Option<DesktopOAuthConfig> {
    let client_id = std::env::var(DESKTOP_CLIENT_ID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            settings
                .google_desktop_client_id
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            let d = DEFAULT_DESKTOP_CLIENT_ID.trim();
            (!d.is_empty()).then(|| d.to_string())
        })?;

    let client_secret = std::env::var(DESKTOP_CLIENT_SECRET_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            settings
                .google_desktop_client_secret
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        });

    Some(DesktopOAuthConfig { client_id, client_secret })
}

pub fn pkce_pair() -> (String, String) {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("system randomness");
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn build_authorization_url(
    client_id: &str,
    state: &str,
    nonce: &str,
    code_challenge: &str,
) -> Result<String> {
    let url = url::Url::parse_with_params(
        GOOGLE_AUTH_URL,
        [
            ("client_id", client_id),
            ("redirect_uri", LOOPBACK_REDIRECT_URI),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
            ("state", state),
            ("nonce", nonce),
        ],
    )?;
    Ok(url.into())
}

#[derive(Debug, PartialEq, Eq)]
pub enum CallbackGrant {
    AuthorizationCode(String),
    IdToken(String),
}

/// Parse an HTTP request arriving on the loopback redirect listener.
pub fn parse_loopback_request(raw: &[u8], expected_state: &str) -> Result<CallbackGrant> {
    let text = std::str::from_utf8(raw).context("OAuth callback was not UTF-8")?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .unwrap_or((text, ""));
    let request_line = head.lines().next().context("empty HTTP request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    ensure!(matches!(method, "GET" | "POST"), "unsupported HTTP method on OAuth callback");

    let parsed_uri = url::Url::parse(&format!("http://localhost:{LOOPBACK_PORT}{target}"))
        .context("malformed callback URI")?;
    ensure!(
        parsed_uri.path() == LOOPBACK_RETURN_PATH,
        "unexpected path {} on loopback listener",
        parsed_uri.path()
    );

    let mut params: HashMap<String, String> = parsed_uri.query_pairs().into_owned().collect();
    if method == "POST" && !body.is_empty() {
        params.extend(url::form_urlencoded::parse(body.as_bytes()).into_owned());
    }

    let state = params.get("state").map(String::as_str).unwrap_or("");
    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        bail!("OAuth state mismatch on loopback return");
    }
    if let Some(err) = params.get("error").filter(|e| !e.is_empty()) {
        bail!("Google sign-in was cancelled or refused ({err})");
    }
    if let Some(id_token) = params.get("id_token").filter(|t| !t.is_empty()) {
        return Ok(CallbackGrant::IdToken(id_token.clone()));
    }
    if let Some(code) = params.get("code").filter(|c| !c.is_empty()) {
        return Ok(CallbackGrant::AuthorizationCode(code.clone()));
    }
    bail!("Google callback included neither an authorization code nor an ID token")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Fetch a Google ID token via the system browser and `http://localhost:47600/oauth/google/return`.
/// Caller must ensure port 47600 is free while this runs (pause the embedded hub first if needed).
pub async fn obtain_google_id_token(cfg: &DesktopOAuthConfig, nonce: &str) -> Result<String> {
    let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, LOOPBACK_PORT)))
        .await
        .with_context(|| format!("binding 127.0.0.1:{LOOPBACK_PORT} for Google OAuth return"))?;
    let v6 = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, LOOPBACK_PORT))).await.ok();

    let state = hub::db::random_hex(16);
    let (verifier, challenge) = pkce_pair();
    let auth_url = build_authorization_url(&cfg.client_id, &state, nonce, &challenge)?;

    open_system_browser(&auth_url)?;

    let grant = tokio::time::timeout(CALLBACK_TIMEOUT, accept_valid_callback(&v4, v6.as_ref(), &state))
        .await
        .context("timed out waiting for Google sign-in in your browser")??;
    drop(v6);
    drop(v4);

    match grant {
        CallbackGrant::IdToken(id_token) => Ok(id_token),
        CallbackGrant::AuthorizationCode(code) => exchange_code_for_id_token(cfg, &code, &verifier).await,
    }
}

async fn accept_valid_callback(
    v4: &TcpListener,
    v6: Option<&TcpListener>,
    expected_state: &str,
) -> Result<CallbackGrant> {
    loop {
        let mut stream = match v6 {
            Some(v6_listener) => {
                tokio::select! {
                    r = v4.accept() => r?.0,
                    r = v6_listener.accept() => r?.0,
                }
            }
            None => v4.accept().await?.0,
        };
        if let Some(result) = handle_callback_stream(&mut stream, expected_state).await {
            return result;
        }
    }
}

async fn handle_callback_stream(stream: &mut TcpStream, expected_state: &str) -> Option<Result<CallbackGrant>> {
    let mut buf = vec![0u8; MAX_HTTP_REQ_BYTES];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await.ok()?.ok()?;
    if n == 0 {
        return None;
    }
    buf.truncate(n);
    // Ignore browser `/favicon.ico` probes while waiting for `/oauth/google/return`.
    if buf.starts_with(b"GET /favicon.ico ") {
        let _ = write_http_html(stream, "404 Not Found", "<p>Not found.</p>").await;
        return None;
    }
    match parse_loopback_request(&buf, expected_state) {
        Ok(grant) => {
            let _ = write_http_html(
                stream,
                "200 OK",
                "<h1>Signed in to Grouper</h1><p>You can close this browser tab and return to Grouper.</p>",
            )
            .await;
            Some(Ok(grant))
        }
        Err(err) => {
            let _ = write_http_html(
                stream,
                "400 Bad Request",
                "<p>Sign-in could not be completed. Return to the Grouper app and try again.</p>",
            )
            .await;
            Some(Err(err))
        }
    }
}

async fn write_http_html(stream: &mut TcpStream, status: &str, body_inner: &str) -> std::io::Result<()> {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Grouper sign-in</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:30rem;margin:4rem auto;padding:0 1rem;color:#18181b}}</style>\
         </head><body>{body_inner}</body></html>"
    );
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await
}

#[derive(Deserialize)]
struct TokenEndpointResponse {
    id_token: Option<String>,
}

async fn exchange_code_for_id_token(
    cfg: &DesktopOAuthConfig,
    code: &str,
    code_verifier: &str,
) -> Result<String> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let mut form = vec![
        ("client_id", cfg.client_id.as_str()),
        ("code", code),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", LOOPBACK_REDIRECT_URI),
    ];
    if let Some(secret) = cfg.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let encoded_form = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        ser.extend_pairs(form);
        ser.finish()
    };
    let resp = http
        .post(GOOGLE_TOKEN_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(encoded_form)
        .send()
        .await
        .context("exchanging Google authorization code")?;
    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let desc = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("token exchange rejected by Google");
        bail!("Google token exchange failed ({status}): {desc}");
    }
    let parsed: TokenEndpointResponse = resp.json().await.context("reading Google token response")?;
    parsed.id_token.filter(|t| !t.is_empty()).context("Google token response did not include an id_token")
}

/// Fetch a hub nonce (`POST /v1/auth/google/nonce`) and sign into a remote hub (`POST /v1/auth/google`),
/// returning the admin device token.
pub async fn sign_in_remote_hub(
    remote_hub_url: &str,
    cfg: &DesktopOAuthConfig,
    device_name: &str,
    install_id: &str,
) -> Result<String> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let base = remote_hub_url.trim_end_matches('/');
    let nonce_resp = http
        .post(format!("{base}/v1/auth/google/nonce"))
        .json(&json!({}))
        .send()
        .await
        .with_context(|| format!("reaching hub at {base}"))?;
    if !nonce_resp.status().is_success() {
        let body: Value = nonce_resp.json().await.unwrap_or(Value::Null);
        let msg = body["error"].as_str().unwrap_or("hub refused to issue a sign-in nonce");
        bail!("{msg}");
    }
    let nonce_json: Value = nonce_resp.json().await.context("reading nonce response")?;
    let nonce = nonce_json["nonce"].as_str().context("hub sent no nonce")?;

    let id_token = obtain_google_id_token(cfg, nonce).await?;

    // See TODO(Q5): `POST /v1/auth/google` currently accepts `"cli"` / `"macos"` / `"ios"`.
    let sign_in_resp = http
        .post(format!("{base}/v1/auth/google"))
        .json(&json!({
            "id_token": id_token,
            "client": "cli",
            "device_name": device_name,
            "install_id": install_id,
        }))
        .send()
        .await
        .with_context(|| format!("completing Google sign-in at {base}"))?;
    if !sign_in_resp.status().is_success() {
        let body: Value = sign_in_resp.json().await.unwrap_or(Value::Null);
        let msg = body["error"].as_str().unwrap_or("hub refused Google sign-in");
        bail!("{msg}");
    }
    let body: Value = sign_in_resp.json().await.context("reading sign-in response")?;
    body["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .context("hub returned no admin device token")
}

fn open_system_browser(url: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching default system browser for Google sign-in")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_client_id_from_config_and_builds_loopback_auth_url() {
        let mut settings = DesktopSettings::default();
        if std::env::var_os(DESKTOP_CLIENT_ID_ENV).is_none() {
            assert!(resolve_oauth_config(&settings).is_none());
            settings.google_desktop_client_id = Some("test-desktop-id.apps.googleusercontent.com".into());
            let resolved = resolve_oauth_config(&settings).unwrap();
            assert_eq!(resolved.client_id, "test-desktop-id.apps.googleusercontent.com");
        }

        let (verifier, challenge) = pkce_pair();
        assert!(verifier.len() >= 43);
        let url_str = build_authorization_url("cid.apps.googleusercontent.com", "st123", "n456", &challenge).unwrap();
        let parsed = url::Url::parse(&url_str).unwrap();
        let qp: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(qp.get("redirect_uri").map(String::as_str), Some("http://localhost:47600/oauth/google/return"));
        assert_eq!(qp.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(qp.get("scope").map(String::as_str), Some("openid email profile"));
        assert_eq!(qp.get("state").map(String::as_str), Some("st123"));
        assert_eq!(qp.get("nonce").map(String::as_str), Some("n456"));
    }

    #[test]
    fn parses_loopback_get_and_rejects_wrong_state() {
        let req = b"GET /oauth/google/return?code=4%2Fabc123&state=expected-state HTTP/1.1\r\nHost: localhost:47600\r\n\r\n";
        let grant = parse_loopback_request(req, "expected-state").unwrap();
        assert_eq!(grant, CallbackGrant::AuthorizationCode("4/abc123".into()));

        let bad_state = b"GET /oauth/google/return?code=abc&state=wrong HTTP/1.1\r\nHost: localhost:47600\r\n\r\n";
        assert!(parse_loopback_request(bad_state, "expected-state").is_err());

        let post_req = b"POST /oauth/google/return HTTP/1.1\r\nHost: localhost:47600\r\n\r\nid_token=eyJ.test.sig&state=expected-state";
        let grant2 = parse_loopback_request(post_req, "expected-state").unwrap();
        assert_eq!(grant2, CallbackGrant::IdToken("eyJ.test.sig".into()));
    }
}
