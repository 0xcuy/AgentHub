use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

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

pub async fn index() -> impl IntoResponse {
    html(LANDING)
}

pub async fn login_page() -> impl IntoResponse {
    html(LOGIN)
}

pub async fn signup_page() -> impl IntoResponse {
    html(SIGNUP)
}

pub async fn app_page() -> impl IntoResponse {
    html(APP)
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        CSS,
    )
}

pub async fn script() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        JS,
    )
}

pub async fn font(axum::extract::Path(file): axum::extract::Path<String>) -> impl IntoResponse {
    let (bytes, content_type): (&[u8], &str) = match file.as_str() {
        "inter.woff2" => (FONT_INTER, "font/woff2"),
        "space-grotesk.woff2" => (FONT_GROTESK, "font/woff2"),
        "instrument-serif.woff2" => (FONT_SERIF, "font/woff2"),
        "favicon.svg" => (FAVICON, "image/svg+xml"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "").into_response(),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
        bytes,
    )
        .into_response()
}

fn html(body: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}
