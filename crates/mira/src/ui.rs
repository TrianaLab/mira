//! The UI, compiled into the binary.
//!
//! Principle 4 says a single binary with no operational overhead. A UI served
//! from a sidecar, a CDN or an unpacked asset directory is a second thing to
//! deploy, version and get out of sync with the API it talks to — so the built
//! bundle is `include_bytes!`d and the binary grows by exactly the size of the
//! files. Three of them, listed by name: Vite is configured with fixed output
//! names and no code splitting, because content hashes in filenames would mean
//! regenerating this table on every build.
//!
//! `dist/` is checked into git. That is the trade: `cargo build` never needs
//! node, and `npm run build` is a step a developer takes before committing a UI
//! change. The alternative — a build.rs that shells out to npm — makes every
//! Rust build depend on a JavaScript toolchain to produce bytes that did not
//! change.
//!
//! Freshness is an ETag over the bytes rather than a hash in the URL. The hash
//! is computed once on first use, the browser sends it back on the next load,
//! and an unchanged bundle costs three 304s — the ETag is per asset, so a page
//! load revalidates each of the three — instead of 60 KB.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

struct Asset {
    name: &'static str,
    mime: &'static str,
    body: &'static [u8],
    etag: LazyLock<String>,
}

macro_rules! asset {
    ($name:literal, $mime:literal) => {{
        // Bound once: naming the bytes keeps `include_bytes!` to a single
        // expansion, so the file is not embedded twice.
        const BODY: &[u8] = include_bytes!(concat!("../ui/dist/", $name));
        Asset {
            name: $name,
            mime: $mime,
            body: BODY,
            etag: LazyLock::new(|| etag(BODY)),
        }
    }};
}

static ASSETS: [Asset; 3] = [
    asset!("index.html", "text/html; charset=utf-8"),
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("app.css", "text/css; charset=utf-8"),
];

pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{file}", get(file))
}

async fn index(headers: HeaderMap) -> Response {
    serve(&ASSETS[0], &headers)
}

/// One of the three assets, or nothing.
///
/// There is no SPA fallback here and there must not be one: every view in the
/// app lives under the hash (`/#/logs`), so a *path* this table does not know is
/// not a UI route that needs rescuing — it is a request for something that does
/// not exist. Answering it with 200 and index.html made `/health`, `/metrics`
/// and every probe path an operator might try report success in HTML, which is
/// the one answer worse than a 404.
async fn file(Path(file): Path<String>, headers: HeaderMap) -> Response {
    match ASSETS.iter().find(|a| a.name == file) {
        Some(a) => serve(a, &headers),
        None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

fn serve(a: &Asset, headers: &HeaderMap) -> Response {
    let etag = a.etag.as_str();
    // `no-cache` means revalidate, not "do not store": the browser keeps the
    // bytes and asks whether they are still current. That is what makes the
    // 304 path the common one after an upgrade.
    let head = [
        (header::CONTENT_TYPE, a.mime),
        (header::ETAG, etag),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, head).into_response();
    }
    (head, a.body).into_response()
}

/// FNV-1a over the bytes, as a quoted ETag.
///
/// A hash of content that cannot change while the process runs needs no
/// collision resistance — only that two different builds differ, which 64 bits
/// of FNV gives for free and without a dependency.
fn etag(body: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}
