use std::net::Ipv4Addr;

use if_addrs::{IfAddr, Interface, get_if_addrs};

const VPN_IFACE_PREFIXES: &[&str] = &[
    "tun", "tap", "docker", "veth", "br-", "wg", "ppp", "utun", "vir", "vgate",
];

const LAN_IFACE_PREFIXES: &[&str] = &["wlan", "eth", "en", "usb"];

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
