use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
};

pub(super) async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}

pub(super) async fn asset(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    match name.as_str() {
        "app.js" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            include_str!("../static/app.js"),
        ),
        "style.css" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            include_str!("../static/style.css"),
        ),
        "samples.json" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            include_str!("../static/samples.json"),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "Not found",
        ),
    }
}

pub(super) async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(req).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
    ] { response.headers_mut().insert(axum::http::HeaderName::from_static(name), axum::http::HeaderValue::from_static(value)); }
    response
}
