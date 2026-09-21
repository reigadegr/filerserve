//! Connection slot registry.
//!
//! Salvo builds the per-request `Request` inside Hyper, and `HyperHandler` is not
//! reachable from outside `salvo_core`, so a crate outside Salvo cannot install a
//! service wrapper that puts the connection's [`SendfileSlot`] into the request
//! extensions. The slot is therefore published under the connection's address
//! pair, which the transport stream and the handler can both observe.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, LazyLock, Mutex},
};

use salvo::conn::SocketAddr;

use crate::body::SendfileSlot;

/// Identifies a connection by its local and remote address.
///
/// A TCP connection is uniquely identified by its four-tuple, so two connections
/// live at the same time never share a key.
pub type ConnKey = (Option<IpAddr>, Option<u16>, Option<IpAddr>, Option<u16>);

static SLOTS: LazyLock<Mutex<HashMap<ConnKey, Arc<SendfileSlot>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Builds the registry key for a connection.
#[must_use]
pub fn conn_key(local: &SocketAddr, remote: &SocketAddr) -> ConnKey {
    (local.ip(), local.port(), remote.ip(), remote.port())
}

pub fn register(key: ConnKey, slot: Arc<SendfileSlot>) {
    if let Ok(mut slots) = SLOTS.lock() {
        slots.insert(key, slot);
    }
}

pub fn unregister(key: ConnKey) {
    if let Ok(mut slots) = SLOTS.lock() {
        slots.remove(&key);
    }
}

/// The sendfile slot of the connection a request arrived on, if any.
///
/// Returns `None` for connections that did not go through
/// [`crate::SendfileListener`], which makes the caller fall back to an ordinary
/// response body.
#[must_use]
pub fn slot_for(local: &SocketAddr, remote: &SocketAddr) -> Option<Arc<SendfileSlot>> {
    SLOTS.lock().ok()?.get(&conn_key(local, remote)).cloned()
}
