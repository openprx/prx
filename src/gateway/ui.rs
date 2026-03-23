use super::AppState;
use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
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
    if !is_safe_asset_path(&path) {
        return not_found();
    }

    let embedded_path = format!("assets/{path}");
    serve_embedded(&embedded_path, CACHE_ASSET)
}

pub async fn app_asset_handler(Path(path): Path<String>) -> Response {
    if !is_safe_asset_path(&path) {
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
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Check if a path is safe for embedded asset serving.
/// Returns `false` for paths containing traversal, backslash, or empty segments.
fn is_safe_asset_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\\') && !path.split('/').any(|segment| segment == ".." || segment.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_safe_asset_path ──────────────────────────────────────

    #[test]
    fn safe_path_normal() {
        assert!(is_safe_asset_path("style.css"));
        assert!(is_safe_asset_path("js/app.mjs"));
        assert!(is_safe_asset_path("img/logo.png"));
    }

    #[test]
    fn safe_path_rejects_empty() {
        assert!(!is_safe_asset_path(""));
    }

    #[test]
    fn safe_path_rejects_dotdot() {
        assert!(!is_safe_asset_path(".."));
        assert!(!is_safe_asset_path("../etc/passwd"));
        assert!(!is_safe_asset_path("js/../../../etc/shadow"));
        assert!(!is_safe_asset_path("foo/bar/../../baz"));
    }

    #[test]
    fn safe_path_rejects_backslash() {
        assert!(!is_safe_asset_path("js\\app.js"));
        assert!(!is_safe_asset_path("..\\..\\etc\\passwd"));
    }

    #[test]
    fn safe_path_rejects_empty_segment() {
        assert!(!is_safe_asset_path("js//app.js"));
        assert!(!is_safe_asset_path("/leading"));
    }

    #[test]
    fn safe_path_allows_dots_in_filenames() {
        assert!(is_safe_asset_path("app.min.js"));
        assert!(is_safe_asset_path("data.v2.json"));
        assert!(is_safe_asset_path(".hidden")); // single-dot is fine (not "..")
    }

    // ── content_type ────────────────────────────────────────────

    #[test]
    fn content_type_html() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
    }

    #[test]
    fn content_type_js() {
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("chunk.mjs"), "text/javascript; charset=utf-8");
    }

    #[test]
    fn content_type_css() {
        assert_eq!(content_type("style.css"), "text/css; charset=utf-8");
    }

    #[test]
    fn content_type_json() {
        assert_eq!(content_type("data.json"), "application/json");
    }

    #[test]
    fn content_type_images() {
        assert_eq!(content_type("logo.svg"), "image/svg+xml");
        assert_eq!(content_type("photo.png"), "image/png");
        assert_eq!(content_type("photo.jpg"), "image/jpeg");
        assert_eq!(content_type("photo.jpeg"), "image/jpeg");
        assert_eq!(content_type("anim.gif"), "image/gif");
        assert_eq!(content_type("img.webp"), "image/webp");
        assert_eq!(content_type("favicon.ico"), "image/x-icon");
    }

    #[test]
    fn content_type_fonts() {
        assert_eq!(content_type("font.woff2"), "font/woff2");
        assert_eq!(content_type("font.woff"), "font/woff");
        assert_eq!(content_type("font.ttf"), "font/ttf");
    }

    #[test]
    fn content_type_sourcemap() {
        assert_eq!(content_type("app.js.map"), "application/json");
    }

    #[test]
    fn content_type_unknown_fallback() {
        assert_eq!(content_type("binary.dat"), "application/octet-stream");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }
}
