// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//
// FluxVM Studio for the fleet central: the built SPA (`ui/fleet/dist`, committed so `cargo build` stays Node-free;
// CI rebuilds and fails if it is stale) embedded and served under `/console/`. Mirrors `fluxvm-api`'s studio.rs
// (the node daemon's UI); the two crates do not depend on each other, so the small server is kept in both.
//
// The files hold no data and no token, so they are served without auth for GET/HEAD; the app asks for a bearer
// token (kept in sessionStorage) and calls `/v1` same-origin, so there is no CORS. Routing in the app is
// hash-based, which means any reverse-proxy prefix works and the only server-side rule is the SPA fallback.

use axum::{
    extract::Path,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};

static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/fleet/dist");

/// Scripts and styles are same-origin files only (no inline script). Inline `style` attributes are used by
/// charts and the segmented control, hence `'unsafe-inline'` for styles, as in the legacy console.
pub const STUDIO_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
     connect-src 'self'; img-src 'self' data: blob:; font-src 'self' data:; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn respond(path: &str, req_headers: &HeaderMap) -> Response {
    let Some(file) = DIST.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = file.contents();
    let hex: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let etag = format!("\"{hex}\"");
    let fresh = req_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let common = [
        (header::CONTENT_SECURITY_POLICY, STUDIO_CSP.to_string()),
        (header::X_FRAME_OPTIONS, "DENY".to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        (header::REFERRER_POLICY, "no-referrer".to_string()),
        // Fixed file names, so always revalidate; the ETag makes that a cheap 304.
        (header::CACHE_CONTROL, "no-cache".to_string()),
        (header::ETAG, etag),
    ];
    if fresh {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    }
    (
        [(header::CONTENT_TYPE, content_type(path).to_string())],
        common,
        body,
    )
        .into_response()
}

/// `/console` -> `/console/`: the app's asset URLs are relative, so the trailing slash is required.
pub async fn studio_root(uri: axum::http::Uri) -> Redirect {
    // Keep the query: Stripe and SSO send people back to `/console?payment_method=added#/billing`.
    Redirect::permanent(&match uri.query() {
        Some(q) => format!("/console/?{q}"),
        None => "/console/".into(),
    })
}

pub async fn studio_index(headers: HeaderMap) -> Response {
    respond("index.html", &headers)
}

/// `/console/{*path}`: an embedded file, or `index.html` for an extension-less path (client-side routes). A missing
/// file with an extension (a stale asset URL) is a real 404, never an HTML page served as JavaScript.
pub async fn studio_file(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let p = path.trim_start_matches('/');
    if p.split('/').any(|s| s == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if DIST.get_file(p).is_some() {
        return respond(p, &headers);
    }
    if p.rsplit('/').next().unwrap_or("").contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    respond("index.html", &headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_build_has_an_entry_point_and_no_inline_script() {
        let html = std::str::from_utf8(DIST.get_file("index.html").expect("index.html").contents())
            .unwrap();
        assert!(
            html.contains("./assets/app.js"),
            "entry script must be relative"
        );
        for (i, _) in html.match_indices("<script") {
            let tag = &html[i..html[i..].find('>').map_or(html.len(), |e| i + e)];
            assert!(
                tag.contains("src="),
                "inline script is blocked by the CSP: {tag}"
            );
        }
        assert!(DIST.get_file("assets/app.js").is_some());
        assert!(DIST.get_file("theme-init.js").is_some());
    }

    #[test]
    fn content_types() {
        assert!(content_type("assets/app.js").starts_with("text/javascript"));
        assert!(content_type("assets/index.css").starts_with("text/css"));
        assert_eq!(content_type("favicon.svg"), "image/svg+xml");
    }
}
