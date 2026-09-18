use std::{net::Ipv4Addr, path::PathBuf};

use if_addrs::{IfAddr, Interface, get_if_addrs};
use salvo::{prelude::*, routing::filters};
use serde::Serialize;

const VPN_IFACE_PREFIXES: &[&str] = &[
    "tun", "tap", "docker", "veth", "br-", "wg", "ppp", "utun", "vir", "vgate",
];

const LAN_IFACE_PREFIXES: &[&str] = &["wlan", "eth", "en", "usb"];

#[derive(Serialize)]
struct ListEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: &'static str,
    size: Option<u64>,
    modified: String,
}

#[derive(Serialize)]
struct ListResponse {
    path: String,
    lan_ip: Option<String>,
    port: u16,
    entries: Vec<ListEntry>,
}

pub struct ListApi {
    root: PathBuf,
    pub port: u16,
}

fn lan_ip_candidate(iface: &Interface) -> Option<Ipv4Addr> {
    let name = iface.name.to_lowercase();
    if VPN_IFACE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return None;
    }
    let IfAddr::V4(v4) = &iface.addr else {
        return None;
    };
    if v4.ip.is_loopback() || v4.ip.is_unspecified() {
        return None;
    }
    let octets = v4.ip.octets();
    if octets[0] == 169 && octets[1] == 254 {
        return None;
    }
    // Skip point-to-point interfaces (e.g. VPN with /32 netmask)
    if v4.netmask == Ipv4Addr::BROADCAST {
        return None;
    }
    Some(v4.ip)
}

#[must_use]
pub fn detect_lan_ip() -> Option<String> {
    let interfaces = get_if_addrs().ok()?;

    // Prefer wlan/eth/en/usb, then any remaining non-VPN interface
    interfaces
        .iter()
        .filter(|iface| {
            let name = iface.name.to_lowercase();
            LAN_IFACE_PREFIXES.iter().any(|p| name.starts_with(p))
        })
        .find_map(lan_ip_candidate)
        .or_else(|| interfaces.iter().find_map(lan_ip_candidate))
        .map(|ip| ip.to_string())
        .or_else(|| {
            // Fallback: UDP socket method (may return VPN address if VPN is active)
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
            socket.connect("8.8.8.8:80").ok()?;
            let addr = socket.local_addr().ok()?;
            Some(addr.ip().to_string())
        })
}

impl ListApi {
    #[must_use]
    pub fn new(root: PathBuf, port: u16) -> Self {
        let canonical_root = root.canonicalize().unwrap_or(root);
        Self {
            root: canonical_root,
            port,
        }
    }
}

#[handler]
impl ListApi {
    #[allow(clippy::unused_async, clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let full = self.root.join(&path);

        let Ok(canonical_target) = full.canonicalize() else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        if !canonical_target.starts_with(&self.root) {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        if !canonical_target.is_dir() {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        let Ok(entries) = std::fs::read_dir(&canonical_target) else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        };

        let mut list_entries: Vec<ListEntry> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let (entry_type, size) = if metadata.is_dir() {
                ("dir", None)
            } else {
                ("file", Some(metadata.len()))
            };
            #[allow(clippy::cast_possible_wrap)]
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                })
                .unwrap_or_default();

            list_entries.push(ListEntry {
                name,
                entry_type,
                size,
                modified,
            });
        }

        list_entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            ("dir", "file") => std::cmp::Ordering::Less,
            ("file", "dir") => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        let display_path = if path.is_empty() {
            "/".to_string()
        } else {
            format!("/{path}")
        };

        let response = ListResponse {
            path: display_path,
            lan_ip: detect_lan_ip(),
            port: self.port,
            entries: list_entries,
        };

        res.render(Json(response));
    }
}

#[must_use]
pub fn list_routes(root: PathBuf, port: u16) -> Router {
    Router::new()
        .push(
            Router::with_path("/api/list")
                .filter(filters::get())
                .goal(ListApi::new(root.clone(), port)),
        )
        .push(
            Router::with_path("/api/list/{**path}")
                .filter(filters::get())
                .goal(ListApi::new(root, port)),
        )
}
