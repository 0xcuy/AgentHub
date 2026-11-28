use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

const LANDING: &str = include_str!("../static/index.html");
const LOGIN: &str = include_str!("../static/login.html");
const SIGNUP: &str = include_str!("../static/signup.html");
const APP: &str = include_str!("../static/app.html");
const CSS: &str = include_str!("../static/app.css");
const JS: &str = include_str!("../static/app.js");
const FONT_INTER: &[u8] = include_bytes!("../static/fonts/inter.woff2");
const FONT_GROTESK: &[u8] = include_bytes!("../static/fonts/space-grotesk.woff2");
const FONT_SERIF: &[u8] = include_bytes!("../static/fonts/instrument-serif.woff2");
const FAVICON: &[u8] = include_bytes!("../static/favicon.svg");

/// Long-lived caching is only safe for a URL that names its own content.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Pages and unversioned asset URLs must be revalidated on every load.
const REVALIDATE: &str = "no-cache";

/// An embedded asset's content identity: `etag` answers conditional requests,
/// `version` stamps the `?v=` URL the HTML serves it under.
struct Asset {
    etag: String,
    version: String,
}

fn identify(bytes: &[u8]) -> Asset {
    let digest = format!("{:x}", Sha256::digest(bytes));
    Asset {
        etag: format!("\"{digest}\""),
        version: digest[..12].to_string(),
    }
}

fn js_asset() -> &'static Asset {
    static ASSET: OnceLock<Asset> = OnceLock::new();
    ASSET.get_or_init(|| identify(JS.as_bytes()))
}

fn css_asset() -> &'static Asset {
    static ASSET: OnceLock<Asset> = OnceLock::new();
    ASSET.get_or_init(|| identify(CSS.as_bytes()))
}

/// app.html with its CSS and JS pinned to the current content hashes, so a
/// deploy changes the URLs the page asks for. A browser or CDN holding the
/// old /app.js cannot keep serving it once the shell asks for a new `?v=`.
fn app_html() -> &'static str {
    static HTML: OnceLock<String> = OnceLock::new();
    HTML.get_or_init(|| {
        APP.replace(
            "/app.css\"",
            &format!("/app.css?v={}\"", css_asset().version),
        )
        .replace("/app.js\"", &format!("/app.js?v={}\"", js_asset().version))
    })
}

pub async fn index(headers: HeaderMap) -> Response {
    page(landing_html(), &headers)
}

fn landing_html() -> &'static str {
    static HTML: OnceLock<String> = OnceLock::new();
    HTML.get_or_init(|| {
        LANDING.replace(
            "/app.css\"",
            &format!("/app.css?v={}\"", css_asset().version),
        )
    })
}

/// A signed-in visitor returning to /login — back button, restored tab, an
/// old bookmark — goes straight to the app instead of being asked to sign in
/// again.
pub async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if signed_in(&state, &headers) {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/app")]).into_response();
    }
    page(LOGIN, &headers)
}

pub async fn signup_page(headers: HeaderMap) -> Response {
    page(SIGNUP, &headers)
}

pub async fn app_page(headers: HeaderMap) -> Response {
    page(app_html(), &headers)
}

pub async fn stylesheet(uri: Uri, headers: HeaderMap) -> Response {
    asset(
        css_asset(),
        CSS.as_bytes(),
        "text/css; charset=utf-8",
        &uri,
        &headers,
    )
}

pub async fn script(uri: Uri, headers: HeaderMap) -> Response {
    asset(
        js_asset(),
        JS.as_bytes(),
        "text/javascript; charset=utf-8",
        &uri,
        &headers,
    )
}

pub async fn font(axum::extract::Path(file): axum::extract::Path<String>) -> impl IntoResponse {
    let (bytes, content_type): (&[u8], &str) = match file.as_str() {
        "inter.woff2" => (FONT_INTER, "font/woff2"),
        "space-grotesk.woff2" => (FONT_GROTESK, "font/woff2"),
        "instrument-serif.woff2" => (FONT_SERIF, "font/woff2"),
        "favicon.svg" => (FAVICON, "image/svg+xml"),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain")],
                "",
            )
                .into_response()
        }
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        bytes,
    )
        .into_response()
}

fn page(body: &'static str, req: &HeaderMap) -> Response {
    let asset = identify(body.as_bytes());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE));
    headers.insert(header::ETAG, etag_value(&asset));
    if not_modified(req, &asset) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    (StatusCode::OK, headers, body).into_response()
}

fn asset(
    meta: &'static Asset,
    bytes: &'static [u8],
    content_type: &'static str,
    uri: &Uri,
    req: &HeaderMap,
) -> Response {
    // Only a URL that pins today's content hash may be cached for good; a
    // bare or stale /app.js must revalidate so the next deploy lands.
    let pinned = uri
        .query()
        .map(|q| {
            q.split('&')
                .any(|kv| kv.strip_prefix("v=") == Some(meta.version.as_str()))
        })
        .unwrap_or(false);
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if pinned { IMMUTABLE } else { REVALIDATE }),
    );
    headers.insert(header::ETAG, etag_value(meta));
    if not_modified(req, meta) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    (StatusCode::OK, headers, bytes).into_response()
}

fn etag_value(asset: &Asset) -> HeaderValue {
    HeaderValue::from_str(&asset.etag).expect("an ETag is always header-safe")
}

fn not_modified(req: &HeaderMap, asset: &Asset) -> bool {
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .any(|t| t == asset.etag || t == "*")
        })
        .unwrap_or(false)
}

fn signed_in(state: &AppState, headers: &HeaderMap) -> bool {
    matches!(crate::api::ui::current_session(state, headers), Ok(Some(_)))
}
