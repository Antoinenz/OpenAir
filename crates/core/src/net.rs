//! Choosing which local network interface to talk to a receiver from.
//!
//! # Why this exists
//!
//! On a multi-homed machine the OS picks the source address for an outgoing
//! connection from its routing table, and that choice can be wrong. Virtual
//! adapters (VMware, Hyper-V, VPNs, phone tethering) frequently install routes
//! that win against the real LAN interface, so a connection to a receiver at
//! `192.168.1.106` can go out with a source address of, say, `192.168.243.92`.
//!
//! The SYN still reaches the receiver, so the connection *looks* like it opens
//! — but the receiver's reply comes back to an address our socket isn't on and
//! the stack resets it. Observed symptom: the receiver accepts the connection,
//! processes `pair-setup`, then its first response write fails with
//! "connection reset by peer", and the session dies before it starts.
//!
//! The same address is advertised in `SETPEERS` for PTP, so a bad choice also
//! points the receiver's clock daemon at an unreachable peer.
//!
//! The fix is to pick the interface whose own subnet contains the receiver and
//! bind to it explicitly, rather than trusting the routing table.

use std::net::{IpAddr, Ipv4Addr};

/// A local IPv4 interface address and its subnet size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalIpv4 {
    pub ip: Ipv4Addr,
    /// CIDR prefix length, e.g. 24 for a 255.255.255.0 netmask.
    pub prefixlen: u8,
}

/// Whether `a` and `b` share a subnet of size `prefixlen`.
fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefixlen: u8) -> bool {
    if prefixlen == 0 {
        // A /0 "subnet" matches everything, which tells us nothing about
        // reachability — treat it as no match rather than a free win.
        return false;
    }
    if prefixlen > 32 {
        return false;
    }
    let mask: u32 = u32::MAX
        .checked_shl(32 - u32::from(prefixlen))
        .unwrap_or(u32::MAX);
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// Addresses that can never be a useful source for reaching a LAN receiver.
fn is_unusable(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast()
}

/// Pick the local address to send from when talking to `dest`.
///
/// Returns the candidate whose subnet contains `dest`, preferring the most
/// specific (longest prefix) match when several qualify — that's the interface
/// genuinely on the receiver's network. Returns `None` when nothing matches, in
/// which case the caller should fall back to letting the OS choose (the
/// receiver may legitimately be off-subnet, behind a router).
pub fn select_source_ipv4(candidates: &[LocalIpv4], dest: Ipv4Addr) -> Option<Ipv4Addr> {
    candidates
        .iter()
        .filter(|c| !is_unusable(c.ip))
        .filter(|c| same_subnet(c.ip, dest, c.prefixlen))
        .max_by_key(|c| c.prefixlen)
        .map(|c| c.ip)
}

/// Enumerate this machine's usable IPv4 interface addresses.
pub fn local_ipv4_addrs() -> Vec<LocalIpv4> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(LocalIpv4 {
                ip: v4.ip,
                prefixlen: v4.prefixlen,
            }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .filter(|c| !is_unusable(c.ip))
        .collect()
}

/// The local address to bind to when connecting to `dest`, or `None` to let
/// the OS decide (`dest` is IPv6, or no local interface shares its subnet).
pub fn source_addr_for(dest: IpAddr) -> Option<IpAddr> {
    let IpAddr::V4(dest_v4) = dest else {
        // IPv6 selection has its own scoping rules; don't second-guess the OS.
        return None;
    };
    select_source_ipv4(&local_ipv4_addrs(), dest_v4).map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lif(ip: [u8; 4], prefixlen: u8) -> LocalIpv4 {
        LocalIpv4 {
            ip: Ipv4Addr::from(ip),
            prefixlen,
        }
    }

    /// The exact failure from the field: a virtual adapter on an unrelated
    /// subnet must never be chosen over the real LAN interface.
    #[test]
    fn prefers_lan_interface_over_virtual_adapter() {
        let candidates = [
            lif([192, 168, 243, 92], 24), // VMware/Hyper-V style virtual adapter
            lif([192, 168, 1, 108], 24),  // real Wi-Fi LAN
        ];
        let picked = select_source_ipv4(&candidates, Ipv4Addr::new(192, 168, 1, 106));
        assert_eq!(picked, Some(Ipv4Addr::new(192, 168, 1, 108)));
    }

    #[test]
    fn order_does_not_matter() {
        let candidates = [
            lif([192, 168, 1, 108], 24),
            lif([192, 168, 243, 92], 24),
        ];
        let picked = select_source_ipv4(&candidates, Ipv4Addr::new(192, 168, 1, 106));
        assert_eq!(picked, Some(Ipv4Addr::new(192, 168, 1, 108)));
    }

    #[test]
    fn prefers_most_specific_subnet() {
        // Both contain the destination; the /24 is the real local segment.
        let candidates = [lif([10, 0, 0, 5], 8), lif([10, 1, 2, 3], 24)];
        let picked = select_source_ipv4(&candidates, Ipv4Addr::new(10, 1, 2, 200));
        assert_eq!(picked, Some(Ipv4Addr::new(10, 1, 2, 3)));
    }

    #[test]
    fn no_match_defers_to_the_os() {
        // Receiver is off-subnet (behind a router) — we must not guess.
        let candidates = [lif([192, 168, 1, 108], 24)];
        assert_eq!(
            select_source_ipv4(&candidates, Ipv4Addr::new(10, 9, 9, 9)),
            None
        );
    }

    #[test]
    fn skips_loopback_and_link_local() {
        let candidates = [
            lif([127, 0, 0, 1], 8),
            lif([169, 254, 3, 4], 16),
            lif([192, 168, 1, 108], 24),
        ];
        // Destination on the APIPA range must not select the link-local addr.
        assert_eq!(
            select_source_ipv4(&candidates, Ipv4Addr::new(169, 254, 9, 9)),
            None
        );
        // And loopback is never offered for a LAN destination.
        assert_eq!(
            select_source_ipv4(&candidates, Ipv4Addr::new(192, 168, 1, 106)),
            Some(Ipv4Addr::new(192, 168, 1, 108))
        );
    }

    #[test]
    fn zero_prefix_is_not_a_match() {
        // A /0 route matches everything and so proves nothing about being on
        // the receiver's network.
        let candidates = [lif([25, 60, 1, 1], 0)];
        assert_eq!(
            select_source_ipv4(&candidates, Ipv4Addr::new(192, 168, 1, 106)),
            None
        );
    }

    #[test]
    fn ipv6_destination_defers_to_the_os() {
        assert_eq!(source_addr_for("::1".parse().unwrap()), None);
    }

    #[test]
    fn exact_host_route_matches() {
        let candidates = [lif([192, 168, 1, 108], 32)];
        assert_eq!(
            select_source_ipv4(&candidates, Ipv4Addr::new(192, 168, 1, 108)),
            Some(Ipv4Addr::new(192, 168, 1, 108))
        );
    }
}
