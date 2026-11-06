//! Public landing page, served by `hashu mint run` at `/` on the mint's
//! HTTP port (next to the Cashu API under `/v1` and the buyer checkout at
//! `/checkout/`). Static HTML; it reads live price and rails from
//! `/checkout/api/info`.

use axum::response::Html;
use axum::routing::get;
use axum::Router;

const PAGE: &str = include_str!("landing.html");

/// `GET /`: the landing page.
pub fn router() -> Router {
    Router::new().route("/", get(|| async { Html(PAGE) }))
}
