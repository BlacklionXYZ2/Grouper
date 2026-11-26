//! The web client, served by the hub from files embedded in the binary.
//!
//! `GET /` and `GET /app` serve the page; `GET /app/<file>` its scripts, styles and
//! icon. The source is plain HTML, CSS and ES modules in `crates/hub/web/` (no build
//! step, no CDN). Every response carries a strict Content-Security-Policy (no inline
//! script or style, nothing from other origins, no framing), `nosniff` and
//! `Referrer-Policy: no-referrer`. The page signs in through `/v1/session`
//! (see [`crate::session`]).

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};

use crate::server::Hub;

/// `connect-src 'self'` covers the event socket too: in CSP Level 3 'self' matches
/// ws:/wss: on the page's own host and port (checked in Edge/Chromium).
pub const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; \
                       script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../web/", $path)) as &[u8])),*]
    };
}

/// Every file of the web client, by its path under `/app/`.
const ASSETS: &[(&str, &[u8])] = assets![
    "index.html",
    "app.css",
    "app.js",
    "icon.svg",
    "lib/api.js",
    "lib/dom.js",
    "lib/family.js",
    "lib/format.js",
    "lib/live.js",
    "lib/md.js",
    "lib/store.js",
    "lib/ui.js",
    "views/approvals.js",
    "views/chat.js",
    "views/devices.js",
    "views/inbox.js",
    "views/login.js",
    "views/memory.js",
    "views/model-picker.js",
    "views/models.js",
    "views/more.js",
    "views/palette.js",
    "views/settings.js",
    "views/skills.js",
    "views/thread.js",
    "views/threads-panel.js",
    "views/usage.js",
];

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn etags() -> &'static [String] {
    static TAGS: OnceLock<Vec<String>> = OnceLock::new();
    TAGS.get_or_init(|| {
        ASSETS.iter().map(|(_, body)| format!("\"{}\"", hex::encode(&Sha256::digest(body)[..12]))).collect()
    })
}

/// The security headers every web client response carries.
pub(crate) fn secure_headers(headers: &mut HeaderMap) {
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
}

fn serve(path: &str, request: &HeaderMap) -> Response {
    let Some(index) = ASSETS.iter().position(|(p, _)| *p == path) else {
        let mut r = (StatusCode::NOT_FOUND, "not found").into_response();
        secure_headers(r.headers_mut());
        return r;
    };
    let etag = &etags()[index];
    let fresh = request
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, content_type(path))], ASSETS[index].1).into_response()
    };
    let h = response.headers_mut();
    secure_headers(h);
    h.insert(header::ETAG, HeaderValue::from_str(etag).expect("hex etag"));
    // Revalidate every time: the files are small and change with the hub binary.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn index(headers: HeaderMap) -> Response {
    serve("index.html", &headers)
}

async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    serve(&path, &headers)
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(|headers: HeaderMap| async move { serve("icon.svg", &headers) }))
        .route("/app", get(index))
        .route("/app/", get(index))
        .route("/app/{*path}", get(asset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_is_served_with_its_type_and_headers() {
        for (path, body) in ASSETS {
            let r = serve(path, &HeaderMap::new());
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            assert_ne!(content_type(path), "application/octet-stream", "{path}");
            assert!(!body.is_empty(), "{path}");
            assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP);
            assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert_eq!(r.headers()[header::REFERRER_POLICY], "no-referrer");
        }
        assert_eq!(serve("../Cargo.toml", &HeaderMap::new()).status(), StatusCode::NOT_FOUND);
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(&etags()[0]).unwrap());
        assert_eq!(serve(ASSETS[0].0, &h).status(), StatusCode::NOT_MODIFIED);
    }

    /// The CSP forbids inline script and style, so the sources must not rely on them.
    #[test]
    fn no_inline_script_or_style() {
        for (path, body) in ASSETS {
            let text = std::str::from_utf8(body).unwrap();
            if path.ends_with(".html") {
                assert!(!text.contains("<style"), "{path} has an inline <style>");
                assert!(!text.contains(" style="), "{path} has an inline style attribute");
                for tag in text.split("<script").skip(1) {
                    assert!(tag.split('>').next().unwrap().contains("src="), "{path} has an inline <script>");
                }
                let handler = text.split(" on").skip(1).any(|rest| {
                    let name: String = rest.chars().take_while(char::is_ascii_lowercase).collect();
                    !name.is_empty() && rest[name.len()..].starts_with('=')
                });
                assert!(!handler, "{path} has an inline event handler");
            }
            if path.ends_with(".js") {
                assert!(!text.contains("innerHTML"), "{path} uses innerHTML");
                assert!(!text.contains("insertAdjacentHTML"), "{path} uses insertAdjacentHTML");
                assert!(!text.contains("eval("), "{path} uses eval");
                assert!(!text.contains("new Function"), "{path} uses new Function");
                assert!(!text.contains("setAttribute('style'"), "{path} sets a style attribute, which the CSP blocks");
            }
        }
    }
}
