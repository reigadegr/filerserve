//! Zero-copy file responses for a Salvo server.
//!
//! Hyper owns the connection socket and writes every response byte itself, so a
//! handler cannot call `sendfile(2)` on its own. This crate bridges that gap:
//!
//! 1. [`SendfileListener`] wraps each accepted connection's transport in a
//!    [`SendfileStream`].
//! 2. [`upgrade_response`] replaces the body of a file response with a
//!    [`SendfileBody`], which reports the file's exact length but yields
//!    placeholder bytes instead of content.
//! 3. The stream recognises those placeholders and issues `sendfile(2)` for the
//!    same length, so the file never enters userspace.
//!
//! Framing is untouched: the placeholder byte count equals the `Content-Length`
//! Hyper was given, so keep-alive, range responses and Hyper's own accounting
//! behave exactly as they do for an ordinary body.
//!
//! # Example
//!
//! ```no_run
//! use salvo::prelude::*;
//!
//! # async fn run() {
//! let acceptor = lanfile_sendfile::SendfileListener::new(TcpListener::new("0.0.0.0:8000"))
//!     .bind()
//!     .await;
//! Server::new(acceptor).serve(Router::new()).await;
//! # }
//! ```

use std::fs::File;

use salvo::{
    http::{
        StatusCode,
        body::ResBody,
        header::{CONTENT_LENGTH, CONTENT_RANGE},
    },
    prelude::*,
};

mod body;
mod conn;
mod registry;
mod stream;

pub use body::{SendfileBody, SendfileSlot};
pub use conn::{SendfileAcceptor, SendfileListener};
pub use registry::{ConnKey, conn_key, slot_for};
pub use stream::{SendfileStream, SendfileTarget};

/// Duplicates an open file so a later [`upgrade_response`] can serve it.
///
/// [`upgrade_response`] must be called after the response has been built, by
/// which point the handler no longer owns the file. Duplicating the handle first
/// is cheaper than re-opening the path: one `dup(2)` instead of a `stat` and an
/// `open(2)`, and it guarantees the same file is served even if the path is
/// replaced in between.
///
/// Returns `None` on platforms without `sendfile(2)`, or when the handle cannot
/// be duplicated; the caller then keeps the ordinary response body.
#[must_use]
pub fn duplicate_file(file: &tokio::fs::File) -> Option<File> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::io::dup(file).ok().map(File::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = file;
        None
    }
}

/// Replaces a file response body with a zero-copy `sendfile(2)` body.
///
/// Call this after the response headers and body have been produced, passing the
/// file that was opened for the same response. The response is left untouched
/// unless every condition holds:
///
/// - the status is `200 OK` or `206 Partial Content`;
/// - `Content-Length` is present and non-zero;
/// - the request arrived on a [`SendfileListener`] connection;
/// - the platform has `sendfile(2)`.
///
/// There is deliberately no size threshold: even for a few kilobytes `sendfile`
/// removes the read into userspace that an ordinary body needs, and the caller
/// is expected to have disabled `NamedFile`'s small-file preload so that read is
/// not paid before this is reached.
///
/// The returned value reports whether the body was replaced.
pub fn upgrade_response(req: &Request, res: &mut Response, file: File) -> bool {
    let status = res.status_code;
    if status != Some(StatusCode::OK) && status != Some(StatusCode::PARTIAL_CONTENT) {
        return false;
    }
    let Some(len) = header_u64(res, CONTENT_LENGTH) else {
        return false;
    };
    let Some(slot) = slot_for(req.local_addr(), req.remote_addr()) else {
        return false;
    };
    let offset = header_str(res, CONTENT_RANGE)
        .and_then(|value| range_start(&value))
        .unwrap_or(0);
    let Some(body) = slot.arm(file, offset, len) else {
        return false;
    };
    res.replace_body(ResBody::Boxed(Box::pin(body)));
    true
}

fn header_str(res: &Response, name: salvo::http::header::HeaderName) -> Option<String> {
    res.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

fn header_u64(res: &Response, name: salvo::http::header::HeaderName) -> Option<u64> {
    header_str(res, name)?.parse().ok()
}

/// Start offset of a `Content-Range: bytes <start>-<end>/<total>` header.
fn range_start(value: &str) -> Option<u64> {
    value
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::range_start;

    #[test]
    fn range_start_parses_a_partial_content_header() {
        assert_eq!(
            range_start("bytes 1048576-2097151/8388608"),
            Some(1_048_576)
        );
        assert_eq!(range_start("bytes 0-99/100"), Some(0));
    }

    #[test]
    fn range_start_rejects_other_forms() {
        assert_eq!(range_start("items 0-99/100"), None);
        assert_eq!(range_start("bytes */100"), None);
    }
}
