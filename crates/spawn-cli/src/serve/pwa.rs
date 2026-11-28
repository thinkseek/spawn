//! The dashboard's PWA shell (issue #91 P1b): the web app manifest, its
//! icon and the service worker, embedded in the binary (`include_str!` of
//! `web/src/pwa/`) and answered by `spawn serve` itself, before any file of
//! the built UI is looked up, so they need no build copy into `web/dist/` and are the
//! same in every deploy of this build.
//!
//! - `/manifest.webmanifest`: `application/manifest+json`.
//! - `/sw.js`: `application/javascript; charset=utf-8`. The worker never
//!   answers nor caches `/api/*`, a request with an `Authorization` header
//!   or a URL with a token (its own doc).
//! - `/icon.svg`: `image/svg+xml`.
//!
//! Each is sent `Cache-Control: no-cache` (a new build's is seen on the
//! next load) with a weak `ETag` of its bytes, and `If-None-Match` gets 304.
//! Only these three exact paths are answered here: any other path is what
//! it was (a built file, else 404; there is no `index.html` fallback, so
//! `/missing.webmanifest` or `/assets/sw.js` stay 404).

use tiny_http::{Header, Request, Response};

use super::{answers, header};

/// One embedded file of the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Asset {
    pub(super) path: &'static str,
    pub(super) content_type: &'static str,
    pub(super) body: &'static str,
}

/// The shell's files, by request path.
pub(super) const ASSETS: [Asset; 3] = [
    Asset {
        path: "/sw.js",
        content_type: "application/javascript; charset=utf-8",
        body: include_str!("../../../../web/src/pwa/sw.js"),
    },
    Asset {
        path: "/manifest.webmanifest",
        content_type: "application/manifest+json",
        body: include_str!("../../../../web/src/pwa/manifest.webmanifest"),
    },
    Asset {
        path: "/icon.svg",
        content_type: "image/svg+xml",
        body: include_str!("../../../../web/src/pwa/icon.svg"),
    },
];

/// Every shell file's `Cache-Control`.
pub(super) const CACHE_CONTROL: &str = "no-cache";

/// The shell file at exactly `path`, if it is one.
pub(super) fn asset(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// Answer `request` with `asset` (module doc).
pub(super) fn respond(request: Request, asset: &Asset) -> Result<(), String> {
    let accept_encoding = header(&request, "Accept-Encoding").map(str::to_string);
    let vary = Header::from_bytes("Vary", "Accept-Encoding").expect("static header is valid");
    let etag = answers::file_etag(asset.body.as_bytes());
    let tag = Header::from_bytes("ETag", etag.as_str()).expect("a hex tag is a valid header");
    let cache = Header::from_bytes("Cache-Control", CACHE_CONTROL).expect("static header is valid");
    if answers::not_modified(header(&request, "If-None-Match"), &etag) {
        return request
            .respond(
                Response::empty(304)
                    .with_header(tag)
                    .with_header(cache)
                    .with_header(vary),
            )
            .map_err(|error| error.to_string());
    }
    let kind =
        Header::from_bytes("Content-Type", asset.content_type).expect("static header is valid");
    let response = if super::gzip::accepts_gzip(accept_encoding.as_deref())
        && let Some(compressed) = super::gzip::compress_if_smaller(asset.body.as_bytes())
    {
        Response::from_data(compressed)
            .with_header(kind)
            .with_header(tag)
            .with_header(cache)
            .with_header(
                Header::from_bytes("Content-Encoding", "gzip").expect("static header is valid"),
            )
            .with_header(vary)
    } else {
        Response::from_data(asset.body.as_bytes())
            .with_header(kind)
            .with_header(tag)
            .with_header(cache)
            .with_header(vary)
    };
    request.respond(response).map_err(|error| error.to_string())
}
