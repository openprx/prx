use super::AppState;
use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;

const INDEX_HTML: &str = "index.html";
const CACHE_INDEX: &str = "no-cache";
const CACHE_ASSET: &str = "public, max-age=31536000, immutable";

#[derive(RustEmbed)]
#[folder = "console/dist/"]
struct ConsoleDist;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index_handler))
        .route("/assets/{*path}", get(asset_handler))
        .route("/_app/{*path}", get(app_asset_handler))
        .fallback(get(spa_fallback_handler))
}

pub async fn index_handler() -> Response {
    serve_embedded(INDEX_HTML, CACHE_INDEX)
}

pub async fn asset_handler(Path(path): Path<String>) -> Response {
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty())
    {
        return not_found();
    }

    let embedded_path = format!("assets/{path}");
    serve_embedded(&embedded_path, CACHE_ASSET)
}

pub async fn app_asset_handler(Path(path): Path<String>) -> Response {
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty())
    {
        return not_found();
    }

    let embedded_path = format!("_app/{path}");
    serve_embedded(&embedded_path, CACHE_ASSET)
}

pub async fn spa_fallback_handler(uri: axum::http::Uri) -> Response {
    // Try serving the exact path as an embedded file first (e.g. /config → config.html)
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && !path.contains('.') {
        let html_path = format!("{path}.html");
        if ConsoleDist::get(&html_path).is_some() {
            return serve_embedded(&html_path, CACHE_INDEX);
        }
    }
    // Fall back to index.html for SPA client-side routing
    serve_embedded(INDEX_HTML, CACHE_INDEX)
}

fn serve_embedded(path: &str, cache_control: &'static str) -> Response {
    let Some(file) = ConsoleDist::get(path) else {
        return not_found();
    };

    let mut response = Response::new(Body::from(file.data.into_owned()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(path)),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".map") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}
