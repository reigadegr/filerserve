//! The per-connection handoff between a handler and the transport stream.

use std::{
    fs::File,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use salvo::http::body::{Body, Frame, SizeHint};

/// Placeholder bytes the response body hands to Hyper in place of file content.
///
/// Hyper insists on writing every response byte itself, so a handler cannot
/// simply call `sendfile(2)`. Instead the body reports the file's exact length
/// and yields this buffer, while [`crate::SendfileStream`] underneath Hyper
/// recognises those bytes and issues the real `sendfile(2)` for the same length.
/// Only the byte count matters, so one shared zero page serves every response.
static PHANTOM: [u8; 256 * 1024] = [0; 256 * 1024];

/// The file range a connection's next response must send with `sendfile(2)`.
pub struct Plan {
    pub file: File,
    pub offset: u64,
    pub remaining: u64,
}

/// Per-connection handoff from a handler to the transport stream.
///
/// A handler arms the slot with the file it is about to serve and returns the
/// [`SendfileBody`] this yields; the connection's [`crate::SendfileStream`]
/// consumes the plan once the response head has reached the socket.
#[derive(Default)]
pub struct SendfileSlot {
    armed: AtomicBool,
    plan: Mutex<Option<Plan>>,
}

impl SendfileSlot {
    /// Creates an empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms this connection's next response to be sent with `sendfile(2)`.
    ///
    /// Returns `None` — and leaves the slot untouched — when the platform has no
    /// `sendfile`, when a plan is already armed, or when `len` is zero. A caller
    /// that gets `None` must keep the ordinary response body.
    pub fn arm(&self, file: File, offset: u64, len: u64) -> Option<SendfileBody> {
        if !cfg!(any(target_os = "linux", target_os = "android")) || len == 0 {
            return None;
        }
        let mut plan = self.plan.lock().ok()?;
        if plan.is_some() {
            return None;
        }
        *plan = Some(Plan {
            file,
            offset,
            remaining: len,
        });
        drop(plan);
        self.armed.store(true, Ordering::Release);
        Some(SendfileBody {
            phantom: Bytes::from_static(&PHANTOM),
            remaining: len,
        })
    }

    /// Whether a plan is waiting to be picked up by the transport stream.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    /// Takes the armed plan, if any, and clears the armed flag.
    ///
    /// The flag is cleared even when there is no plan: once the stream has
    /// committed to reading placeholders it owns the plan, and leaving the slot
    /// armed would make the next response on the connection look like a file.
    pub fn take_plan(&self) -> Option<Plan> {
        let plan = self.plan.lock().ok()?.take();
        self.armed.store(false, Ordering::Release);
        plan
    }
}

/// A response body that makes Hyper emit exactly `len` bytes for a `sendfile`.
///
/// The bytes it yields are placeholders; [`crate::SendfileStream`] replaces them
/// with the file content on the way to the socket.
pub struct SendfileBody {
    phantom: Bytes,
    remaining: u64,
}

impl SendfileBody {
    /// Number of file bytes this body stands in for.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.remaining
    }

    /// Always false: an armed plan is never empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

impl Body for SendfileBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        let len = this.remaining.min(this.phantom.len() as u64) as usize;
        this.remaining -= len as u64;
        Poll::Ready(Some(Ok(Frame::data(this.phantom.slice(0..len)))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}
