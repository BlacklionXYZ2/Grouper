//! The web client, served by the hub from files embedded in the binary.
//!
//! `GET /` and `GET /app` serve the page; `GET /app/<file>` its scripts, styles,
//! icon and web app manifest. `GET /sw.js` serves the service worker (approval
//! notifications only: no fetch handler, no cache) from the site root, so its scope
//! covers the whole app; a worker's scope can't reach above its own path. The source is plain HTML, CSS and ES modules in `crates/hub/web/` (no build
//! step, no CDN). Every response carries a strict Content-Security-Policy (no inline
//! script or style, nothing from other origins, no framing), `nosniff` and
//! `Referrer-Policy: no-referrer`. The page signs in through `/v1/session`
//! (see [`crate::session`]).

use std::sync::OnceLock;

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};

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
    // Per-area stylesheets, linked after app.css (one owner each; see docs/gui-concept-plan.md).
    "css/chat.css",
    "css/thread.css",
    "css/approval.css",
    "css/models.css",
    "css/brand.css",
    "css/link.css",
    "css/hub.css",
    "css/usage.css",
    "css/statusline.css",
    "icon.svg",
    "logo.svg",
    "logo-glyph.svg",
    "manifest.webmanifest",
    "privacy.css",
    "privacy.html",
    "sw.js",
    "lib/api.js",
    "lib/dom.js",
    "lib/family.js",
    "lib/format.js",
    "lib/live.js",
    "lib/md.js",
    "lib/push.js",
    "lib/store.js",
    "lib/ui.js",
    "views/approvals.js",
    "views/chat.js",
    "views/devices.js",
    "views/inbox.js",
    "views/key-color.js",
    "views/link.js",
    "views/login.js",
    "views/memory.js",
    "views/model-picker.js",
    "views/models.js",
    "views/more.js",
    "views/palette.js",
    "views/statusline.js",
    "views/settings.js",
    "views/skills.js",
    "views/thread.js",
    "views/threads-panel.js",
    "views/usage-breakdown.js",
    "views/usage-common.js",
    "views/usage-limits.js",
    "views/usage.js",
];

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("webmanifest") => "application/manifest+json",
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

/// `GET /sw.js`: the service worker, at the root so it may control `/` and `/app`.
async fn service_worker(headers: HeaderMap) -> Response {
    serve("sw.js", &headers)
}

pub(crate) fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(index))
        .route("/sw.js", get(service_worker))
        // The public privacy policy (linked from Grouper's Google sign-in consent screen).
        .route("/privacy", get(|headers: HeaderMap| async move { serve("privacy.html", &headers) }))
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

    /// The service worker is served from the site root as JavaScript (a worker's
    /// scope can't reach above its own path, and browsers refuse other types), with
    /// the same headers as every asset; the manifest has its own type.
    #[tokio::test]
    async fn service_worker_is_served_from_the_root() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, routes::<()>()).await.unwrap() });
        let http = reqwest::Client::new();

        let r = http.get(format!("http://{addr}/sw.js")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "text/javascript; charset=utf-8");
        assert_eq!(r.headers()["content-security-policy"], CSP);
        assert_eq!(r.headers()["cache-control"], "no-cache");
        let body = r.text().await.unwrap();
        assert!(body.contains("addEventListener('push'") && body.contains("addEventListener('notificationclick'"));
        // A network-free worker: it must not intercept requests or cache anything.
        assert!(!body.contains("addEventListener('fetch'") && !body.contains("caches."));

        let r = http.get(format!("http://{addr}/app/manifest.webmanifest")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "application/manifest+json");
        let manifest: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
        assert_eq!(manifest["scope"], "/");
        assert_eq!(manifest["display"], "standalone");
    }

    /// Every app stylesheet is linked from the page, app.css first (the area files
    /// build on its tokens), and every linked file is one the hub serves.
    #[test]
    fn index_links_every_stylesheet_after_app_css() {
        let index = std::str::from_utf8(ASSETS.iter().find(|(p, _)| *p == "index.html").unwrap().1).unwrap();
        let links: Vec<&str> = index
            .split("<link rel=\"stylesheet\" href=\"/app/")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(links.first(), Some(&"app.css"));
        for (path, _) in ASSETS.iter().filter(|(p, _)| p.ends_with(".css") && *p != "privacy.css") {
            assert!(links.contains(path), "{path} is served but not linked from index.html");
        }
        for link in &links {
            assert!(ASSETS.iter().any(|(p, _)| p == link), "index.html links {link}, which isn't served");
        }
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
