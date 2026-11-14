//! LAN address discovery, port selection and QR codes.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};

/// How many consecutive ports are tried when the preferred one is taken.
const PORT_ATTEMPTS: u16 = 32;

/// Interface name prefixes of virtual adapters (containers, hypervisors,
/// VPNs). A headset on the Wi-Fi cannot reach those, so their addresses are
/// listed after the physical ones.
const VIRTUAL_PREFIXES: [&str; 14] = [
    "docker",
    "br-",
    "veth",
    "virbr",
    "vmnet",
    "vboxnet",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "utun",
    "zt",
    "lxc",
    "podman",
];

/// Rank of an address for the URL list: lower is more plausible as "the
/// address a headset on the same network should use".
fn plausibility(name: &str, ip: Ipv4Addr) -> u8 {
    let name = name.to_ascii_lowercase();
    let virtual_adapter = VIRTUAL_PREFIXES.iter().any(|prefix| name.starts_with(prefix));
    let [a, b, ..] = ip.octets();
    let class = match (a, b) {
        _ if ip.is_link_local() => 6,
        // Home and office networks, most common first.
        (192, 168) => 0,
        (10, _) => 1,
        (172, 16..=31) => 2,
        // Carrier-grade NAT, also used by overlay VPNs.
        (100, 64..=127) => 4,
        _ => 3,
    };
    if virtual_adapter {
        class + 10
    } else {
        class
    }
}

/// Orders `(interface name, address)` pairs most plausible first, without
/// duplicates and without loopback or unspecified addresses.
fn rank_addresses(mut candidates: Vec<(String, Ipv4Addr)>) -> Vec<Ipv4Addr> {
    candidates.retain(|(_, ip)| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_broadcast());
    // Stable: interfaces of equal rank keep the order the system lists them in.
    candidates.sort_by_key(|(name, ip)| plausibility(name, *ip));
    let mut out: Vec<Ipv4Addr> = Vec::with_capacity(candidates.len());
    for (_, ip) in candidates {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Every non-loopback IPv4 address of this machine, the most plausible LAN
/// address first (private ranges on physical adapters, then the rest, then
/// virtual adapters and link-local addresses).
pub fn lan_ipv4() -> Vec<Ipv4Addr> {
    let candidates = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter(|iface| !iface.is_loopback())
            .filter_map(|iface| match iface.ip() {
                IpAddr::V4(ip) => Some((iface.name, ip)),
                IpAddr::V6(_) => None,
            })
            .collect(),
        Err(err) => {
            log::warn!("impossible de lister les interfaces réseau : {err}");
            Vec::new()
        }
    };
    rank_addresses(candidates)
}

/// Binds `ip:port`, or the next free port above it when it is taken (up to
/// 32 attempts). Port 0 asks the system for any free port.
pub(crate) fn bind_near(ip: IpAddr, port: u16) -> io::Result<TcpListener> {
    let mut last_error = None;
    for offset in 0..PORT_ATTEMPTS {
        let Some(candidate) = port.checked_add(offset) else { break };
        match TcpListener::bind(SocketAddr::new(ip, candidate)) {
            Ok(listener) => return Ok(listener),
            // Only "taken" is worth another port; anything else (no
            // permission, no such address) would fail the same way.
            Err(err) if err.kind() == io::ErrorKind::AddrInUse && port != 0 => last_error = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free port")))
}

/// QR code of `text` as a standalone SVG document. `None` when the text is
/// too long for a QR code (about 2 kB).
pub fn qr_svg(text: &str) -> Option<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}

/// QR code of `text` drawn with unicode half blocks, for a terminal. Dark
/// modules are printed as spaces on a light background, which scans on the
/// usual dark terminal themes. `None` when the text is too long.
pub fn qr_terminal(text: &str) -> Option<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    Some(code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(list: &[(&str, [u8; 4])]) -> Vec<Ipv4Addr> {
        rank_addresses(list.iter().map(|(name, ip)| (name.to_string(), Ipv4Addr::from(*ip))).collect())
    }

    #[test]
    fn physical_private_addresses_come_first() {
        let order = ranked(&[
            ("lo", [127, 0, 0, 1]),
            ("docker0", [172, 17, 0, 1]),
            ("tailscale0", [100, 101, 102, 103]),
            ("eth1", [169, 254, 3, 4]),
            ("eth0", [203, 0, 113, 9]),
            ("enp3s0", [10, 0, 0, 12]),
            ("wlan0", [192, 168, 1, 20]),
            ("br-1a2b", [192, 168, 49, 1]),
            ("wlan0", [192, 168, 1, 20]),
            ("any", [0, 0, 0, 0]),
        ]);
        let expected: Vec<Ipv4Addr> = [
            [192, 168, 1, 20],
            [10, 0, 0, 12],
            [203, 0, 113, 9],
            [169, 254, 3, 4],
            [192, 168, 49, 1],
            [172, 17, 0, 1],
            [100, 101, 102, 103],
        ]
        .into_iter()
        .map(Ipv4Addr::from)
        .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn this_machine_lists_no_loopback() {
        for ip in lan_ipv4() {
            assert!(!ip.is_loopback() && !ip.is_unspecified());
        }
    }

    #[test]
    fn busy_port_moves_to_the_next_one() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let first = bind_near(ip, 0).unwrap();
        let taken = first.local_addr().unwrap().port();
        if taken == u16::MAX {
            return;
        }
        let second = bind_near(ip, taken).unwrap();
        let got = second.local_addr().unwrap().port();
        assert!(got > taken && got < taken.saturating_add(PORT_ATTEMPTS), "{taken} then {got}");
    }

    #[test]
    fn qr_codes_render() {
        let svg = qr_svg("https://192.168.1.20:7879").unwrap();
        assert!(svg.starts_with("<?xml") && svg.contains("<svg") && svg.trim_end().ends_with("</svg>"));
        let blocks = qr_terminal("https://192.168.1.20:7879").unwrap();
        assert!(blocks.lines().count() >= 10);
        assert!(blocks.chars().any(|c| c == '█' || c == '▀' || c == '▄'));
    }
}
