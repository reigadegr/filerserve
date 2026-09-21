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

static SLOTS: LazyLock<[Shard; SHARDS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(HashMap::new())));

/// Number of independently locked maps.
///
/// Every file response looks its connection's slot up here, so a single map would
/// put one lock — and one cache line — in the way of all worker threads at once.
const SHARDS: usize = 16;

/// One shard of the registry.
type Shard = Mutex<HashMap<ConnKey, Arc<SendfileSlot>>>;

/// The map a connection belongs to.
///
/// The peer port is what tells one connection from another — the local address is
/// the same for all of them — and a client opening many connections hands out
/// consecutive ports, so its low bits spread them evenly.
fn shard(key: &ConnKey) -> &'static Shard {
    let peer_port = key.3.unwrap_or(0);
    &SLOTS[usize::from(peer_port) % SHARDS]
}

/// Builds the registry key for a connection.
#[must_use]
pub fn conn_key(local: &SocketAddr, remote: &SocketAddr) -> ConnKey {
    (local.ip(), local.port(), remote.ip(), remote.port())
}

pub fn register(key: ConnKey, slot: Arc<SendfileSlot>) {
    if let Ok(mut slots) = shard(&key).lock() {
        slots.insert(key, slot);
    }
}

pub fn unregister(key: ConnKey) {
    if let Ok(mut slots) = shard(&key).lock() {
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
    let key = conn_key(local, remote);
    shard(&key).lock().ok()?.get(&key).cloned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{
        net::{Ipv4Addr, SocketAddr as StdSocketAddr},
        sync::Arc,
    };

    use salvo::conn::SocketAddr;

    use super::{SHARDS, conn_key, register, shard, slot_for, unregister};
    use crate::body::SendfileSlot;

    fn addrs(peer_port: u16) -> (SocketAddr, SocketAddr) {
        let local = StdSocketAddr::from((Ipv4Addr::LOCALHOST, 8000)).into();
        let remote = StdSocketAddr::from((Ipv4Addr::LOCALHOST, peer_port)).into();
        (local, remote)
    }

    #[test]
    fn a_registered_slot_is_found_again() {
        let (local, remote) = addrs(40_000);
        let key = conn_key(&local, &remote);
        let slot = Arc::new(SendfileSlot::new());
        register(key, slot);
        assert!(slot_for(&local, &remote).is_some());
        unregister(key);
        assert!(slot_for(&local, &remote).is_none());
    }

    #[test]
    fn consecutive_peer_ports_spread_over_every_shard() {
        // Registering and looking up must agree on the shard, and no single shard
        // may be left carrying the traffic of a whole benchmark.
        let mut shards = std::collections::HashSet::new();
        for peer_port in 40_000..40_000 + u16::try_from(SHARDS).unwrap() {
            let (local, remote) = addrs(peer_port);
            let key = conn_key(&local, &remote);
            shards.insert(std::ptr::from_ref(shard(&key)));
        }
        assert_eq!(shards.len(), SHARDS);
    }
}
