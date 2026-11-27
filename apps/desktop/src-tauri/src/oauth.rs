//! Desktop Google OAuth 2.0 sign-in. The return listener binds `127.0.0.1` on a free port
//! (`http://127.0.0.1:<port>/oauth/google/return`), which a Desktop-type client may use without
//! a registered redirect. It must not use port 47600: that is the embedded hub.
//!
//! TODO(Desktop OAuth client): the owner has not created the Desktop client yet.
//!   1. Keep `DEFAULT_DESKTOP_CLIENT_ID` empty until that client exists. Until then set
//!      `GROUPER_GOOGLE_DESKTOP_CLIENT_ID` (and `GROUPER_GOOGLE_CLIENT_IDS`, which replaces the
//!      hub's default audience list).
//!   2. When the id is set, add the same id to `DEFAULT_CLIENT_IDS` in `crates/hub/src/google_auth.rs`.
//!
//! The client secret is never a plain field in `desktop.json`. Read it from the DPAPI setting,
//! or from `GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET` when that setting is absent.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
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
/// Filled in once the owner creates the Desktop OAuth client in Google Cloud Console.
/// TODO: leave this empty until that client exists. When it is set, add the same id to
/// `DEFAULT_CLIENT_IDS` in `crates/hub/src/google_auth.rs`.
pub const DEFAULT_DESKTOP_CLIENT_ID: &str = "";

/// Hub listen port. Google's return listener must not bind this.
const HUB_PORT: u16 = 47600;
pub const LOOPBACK_RETURN_PATH: &str = "/oauth/google/return";

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

    // DPAPI setting first. The env var is only the fallback when nothing is stored.
    let client_secret = settings.google_desktop_client_secret().ok().flatten().filter(|v| !v.is_empty()).or_else(|| {
        std::env::var(DESKTOP_CLIENT_SECRET_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
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
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    code_challenge: &str,
) -> Result<String> {
    let url = url::Url::parse_with_params(
        GOOGLE_AUTH_URL,
        [
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
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

/// One-shot loopback listener for Google's redirect. Binds a free port on `127.0.0.1` and
/// closes after the first OAuth return so it never occupies the hub's port.
pub struct GoogleReturnListener {
    listener: TcpListener,
    port: u16,
}

impl GoogleReturnListener {
    pub async fn bind() -> Result<Self> {
        for _ in 0..8 {
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .context("binding 127.0.0.1:0 for Google OAuth return")?;
            let port = listener.local_addr().context("reading Google OAuth return port")?.port();
            if port != 0 && port != HUB_PORT {
                return Ok(Self { listener, port });
            }
        }
        bail!("could not bind a loopback port other than the hub port {HUB_PORT}");
    }

    /// `http://127.0.0.1:<port>/oauth/google/return` — a loopback redirect with a path, valid for
    /// a Desktop-type OAuth client (no redirect URI is registered in Google Cloud).
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{port}{LOOPBACK_RETURN_PATH}", port = self.port)
    }

    /// Answer Google's redirect, then drop the socket. Browser favicon probes are ignored and
    /// do not count as the return. Any other request ends the listener.
    pub async fn accept_return(self, expected_state: &str) -> Result<CallbackGrant> {
        loop {
            let (mut stream, _) = self.listener.accept().await.context("accepting Google OAuth return")?;
            if let Some(result) = handle_callback_stream(&mut stream, expected_state).await {
                return result;
            }
        }
    }
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

    let parsed_uri = url::Url::parse(&format!("http://127.0.0.1{target}")).context("malformed callback URI")?;
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

/// Fetch a Google ID token via the system browser and a one-shot `127.0.0.1` redirect.
/// The embedded hub can keep listening on port 47600 while this runs.
pub async fn obtain_google_id_token(cfg: &DesktopOAuthConfig, nonce: &str) -> Result<String> {
    let listener = GoogleReturnListener::bind().await?;
    let redirect_uri = listener.redirect_uri();
    let state = hub::db::random_hex(16);
    let (verifier, challenge) = pkce_pair();
    let auth_url = build_authorization_url(&cfg.client_id, &redirect_uri, &state, nonce, &challenge)?;

    open_system_browser(&auth_url)?;

    let grant = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept_return(&state))
        .await
        .context("timed out waiting for Google sign-in in your browser")??;

    match grant {
        CallbackGrant::IdToken(id_token) => Ok(id_token),
        CallbackGrant::AuthorizationCode(code) => {
            exchange_code_for_id_token(cfg, &code, &verifier, &redirect_uri).await
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
    redirect_uri: &str,
) -> Result<String> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let mut form = vec![
        ("client_id", cfg.client_id.as_str()),
        ("code", code),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
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

    let sign_in_resp = http
        .post(format!("{base}/v1/auth/google"))
        .json(&json!({
            "id_token": id_token,
            "client": "windows",
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
        let redirect = "http://127.0.0.1:54321/oauth/google/return";
        let url_str =
            build_authorization_url("cid.apps.googleusercontent.com", redirect, "st123", "n456", &challenge).unwrap();
        let parsed = url::Url::parse(&url_str).unwrap();
        let qp: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(qp.get("redirect_uri").map(String::as_str), Some(redirect));
        assert!(!url_str.contains(":47600"));
        assert_eq!(qp.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(qp.get("scope").map(String::as_str), Some("openid email profile"));
        assert_eq!(qp.get("state").map(String::as_str), Some("st123"));
        assert_eq!(qp.get("nonce").map(String::as_str), Some("n456"));
    }

    #[tokio::test]
    async fn loopback_listener_uses_a_random_port_and_stops_after_one_request() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = GoogleReturnListener::bind().await.unwrap();
        let other = GoogleReturnListener::bind().await.unwrap();
        let port = listener.port;
        assert_ne!(port, 0);
        assert_ne!(port, HUB_PORT);
        assert_ne!(other.port, 0);
        assert_ne!(other.port, HUB_PORT);
        assert_ne!(port, other.port);
        assert_eq!(listener.redirect_uri(), format!("http://127.0.0.1:{port}{LOOPBACK_RETURN_PATH}"));
        drop(other);

        let task = tokio::spawn(async move { listener.accept_return("expected-state").await });
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "GET /oauth/google/return?code=abc123&state=expected-state HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 2048];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");

        let grant = task.await.unwrap().unwrap();
        assert_eq!(grant, CallbackGrant::AuthorizationCode("abc123".into()));

        let mut stopped = false;
        for _ in 0..20 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(stopped, "listener must stop after one Google return");
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
