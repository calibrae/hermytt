use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;

const TERMINAL_HTML: &str = include_str!("../static/terminal.html");
const ADMIN_HTML: &str = include_str!("../static/admin.html");

const XTERM_JS: &[u8] = include_bytes!("../static/vendor/xterm.min.js");
const XTERM_CSS: &[u8] = include_bytes!("../static/vendor/xterm.min.css");
const ADDON_FIT_JS: &[u8] = include_bytes!("../static/vendor/addon-fit.min.js");
const ADDON_WEBLINKS_JS: &[u8] = include_bytes!("../static/vendor/addon-web-links.min.js");

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(terminal))
        .route("/terminal", get(terminal))
        .route("/admin", get(admin))
        .route("/vendor/xterm.min.js", get(xterm_js))
        .route("/vendor/xterm.min.css", get(xterm_css))
        .route("/vendor/addon-fit.min.js", get(addon_fit_js))
        .route("/vendor/addon-web-links.min.js", get(addon_weblinks_js))
}

async fn terminal() -> Html<&'static str> {
    Html(TERMINAL_HTML)
}

async fn admin() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

async fn xterm_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], XTERM_JS)
}

async fn xterm_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], XTERM_CSS)
}

async fn addon_fit_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], ADDON_FIT_JS)
}

async fn addon_weblinks_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], ADDON_WEBLINKS_JS)
}
