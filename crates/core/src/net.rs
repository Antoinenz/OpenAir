//! Knowing which local address we talk to a receiver from.
//!
//! # Why this exists
//!
//! On a multi-homed machine the source address of an outgoing connection
//! decides whether the session works at all: the receiver's replies go back to
//! it, and it's also what we advertise in `SETPEERS` for the receiver's clock
//! daemon. Get it wrong and the receiver accepts the connection, processes
//! `pair-setup`, then its first response write fails with "connection reset by
//! peer" — the session dies before it starts, with nothing logged receiver-side
//! to explain why.
//!
//! # Trust the routing table
//!
//! An earlier version of this module tried to *out-guess* the OS by picking the
//! interface whose subnet contained the receiver. That was a mistake. Real
//! machines have interfaces with overlapping masks — e.g. Wi-Fi on
//! `192.168.243.92/16` and Ethernet on `192.168.1.108/16`, where **both** claim
//! `192.168.0.0/16` and both "match" a receiver at `192.168.1.106`. Subnet
//! matching can't separate them, while the routing table can (and did: Ethernet
//! at interface metric 25 beats Wi-Fi at 35). Second-guessing it turned a
//! working configuration into a broken one.
//!
//! So we ask the OS what source address *it* would use, and use that. The value
//! is not in overriding the decision — it's in **knowing** the decision, so PTP
//! can bind to the same address the RTSP connection uses and `SETPEERS` can
//! advertise something true. [`set_source_override`] exists for the rare setup
//! where the routing table really is wrong.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::OnceLock;

use tracing::{debug, warn};

/// User-forced source address (`--bind`), overriding automatic selection.
static SOURCE_OVERRIDE: OnceLock<IpAddr> = OnceLock::new();

/// Force all outgoing receiver connections to originate from `ip`, bypassing
/// interface selection. For users whose setup we guess wrong. First call wins.
pub fn set_source_override(ip: IpAddr) {
    let _ = SOURCE_OVERRIDE.set(ip);
}

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

/// Local addresses that look like they're on `dest`'s network but aren't the
/// one we're actually using — i.e. plausible `--bind` suggestions when a
/// connection fails.
///
/// This is deliberately only a *hint*. It is not used to choose the source
/// address (see the module docs: subnet matching cannot reliably separate
/// interfaces with overlapping masks, and the routing table already knows
/// better). It exists to turn an opaque connection reset into a next step.
pub fn alternative_sources(candidates: &[LocalIpv4], dest: Ipv4Addr, in_use: Ipv4Addr) -> Vec<Ipv4Addr> {
    let mut alts: Vec<&LocalIpv4> = candidates
        .iter()
        .filter(|c| !is_unusable(c.ip))
        .filter(|c| c.ip != in_use)
        .filter(|c| same_subnet(c.ip, dest, c.prefixlen))
        .collect();
    // Closest-looking first: the more leading bits an address shares with the
    // receiver, the more likely it's on the same physical segment.
    alts.sort_by_key(|c| std::cmp::Reverse(common_prefix_len(c.ip, dest)));
    alts.into_iter().map(|c| c.ip).collect()
}

/// Number of leading bits `a` and `b` share.
fn common_prefix_len(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    (u32::from(a) ^ u32::from(b)).leading_zeros()
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
    if let Some(forced) = SOURCE_OVERRIDE.get() {
        return Some(*forced);
    }
    os_source_for(dest)
}

/// Ask the OS which local address it would send to `dest` from, without
/// sending anything.
///
/// Connecting a UDP socket only sets its default destination — no packets
/// leave the machine — but it makes the kernel run its route lookup, after
/// which `local_addr` reports the source it chose. This is the same decision a
/// TCP connect would make, obtained up front so PTP and `SETPEERS` can agree
/// with it.
pub fn os_source_for(dest: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if dest.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let sock = std::net::UdpSocket::bind(bind).ok()?;
    // Port is arbitrary; only the route lookup matters.
    sock.connect(SocketAddr::new(dest, 9)).ok()?;
    let local = sock.local_addr().ok()?.ip();
    (!local.is_unspecified()).then_some(local)
}

/// A next step to suggest when a receiver connection fails, or `None` if
/// nothing about the local networking looks suspicious.
///
/// Aimed at the case that is otherwise invisible: we're sourcing from an
/// interface on a different physical segment than the receiver, the connection
/// half-opens, and the receiver's first response write is reset — leaving a
/// bare "connection forcibly closed" with no clue where to look.
pub fn connection_hint(dest: IpAddr) -> Option<String> {
    let (IpAddr::V4(dest_v4), Some(IpAddr::V4(src_v4))) = (dest, os_source_for(dest)) else {
        return None;
    };
    let alts = alternative_sources(&local_ipv4_addrs(), dest_v4, src_v4);
    let better = alts.first()?;
    // Only worth mentioning if the alternative really is closer to the
    // receiver than what we're using.
    if common_prefix_len(*better, dest_v4) <= common_prefix_len(src_v4, dest_v4) {
        return None;
    }
    Some(format!(
        "connecting from {src_v4}, but {better} looks closer to {dest_v4}'s network \
         — if this keeps failing, try --bind {better}"
    ))
}

/// Connect to `dest` from the interface that can actually reach it.
///
/// Use this instead of [`TcpStream::connect`] for every receiver connection:
/// the source address we end up with is also what gets advertised to the
/// receiver in `SETPEERS`, so an OS mis-selection breaks PTP as well as RTSP.
///
/// Falls back to a plain connect when no interface matches (the receiver may
/// legitimately be routed) or if binding fails, so this can only ever improve
/// on the default behaviour.
pub fn connect_from_best_source(dest: SocketAddr) -> std::io::Result<TcpStream> {
    let Some(src) = source_addr_for(dest.ip()) else {
        debug!(%dest, "no matching local interface; letting the OS choose the source");
        return TcpStream::connect(dest);
    };
    if src.is_ipv4() != dest.is_ipv4() {
        return TcpStream::connect(dest);
    }

    match bind_and_connect(src, dest) {
        Ok(stream) => {
            debug!(%dest, %src, "connected from selected local address");
            Ok(stream)
        }
        Err(e) => {
            // Binding is an optimisation over the OS default, never a
            // requirement — a failure here must not cost us the connection.
            warn!(%dest, %src, "bind to selected source failed ({e}); falling back to OS routing");
            TcpStream::connect(dest)
        }
    }
}

fn bind_and_connect(src: IpAddr, dest: SocketAddr) -> std::io::Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if dest.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.bind(&SocketAddr::new(src, 0).into())?;
    sock.connect(&dest.into())?;
    Ok(sock.into())
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

    /// The real-world layout that broke the earlier subnet-matching approach:
    /// two interfaces on DIFFERENT networks both carrying a /16, so both
    /// "contain" the receiver. Subnet matching cannot separate these — which is
    /// why selection now belongs to the routing table, and this function only
    /// suggests alternatives.
    #[test]
    fn suggests_the_closer_interface_when_masks_overlap() {
        let candidates = [
            lif([192, 168, 243, 92], 16), // Wi-Fi, different segment
            lif([192, 168, 1, 108], 16),  // Ethernet, receiver's segment
        ];
        let alts = alternative_sources(
            &candidates,
            Ipv4Addr::new(192, 168, 1, 106),
            Ipv4Addr::new(192, 168, 243, 92), // currently in use
        );
        assert_eq!(alts.first(), Some(&Ipv4Addr::new(192, 168, 1, 108)));
    }

    #[test]
    fn does_not_suggest_the_address_already_in_use() {
        let candidates = [lif([192, 168, 1, 108], 16)];
        let alts = alternative_sources(
            &candidates,
            Ipv4Addr::new(192, 168, 1, 106),
            Ipv4Addr::new(192, 168, 1, 108),
        );
        assert!(alts.is_empty());
    }

    #[test]
    fn suggests_nothing_for_an_off_network_receiver() {
        // Receiver is routed, not on any local segment — no advice to give.
        let candidates = [lif([192, 168, 1, 108], 24)];
        let alts = alternative_sources(
            &candidates,
            Ipv4Addr::new(10, 9, 9, 9),
            Ipv4Addr::new(192, 168, 1, 108),
        );
        assert!(alts.is_empty());
    }

    #[test]
    fn never_suggests_loopback_or_link_local() {
        let candidates = [lif([127, 0, 0, 1], 8), lif([169, 254, 3, 4], 16)];
        let alts = alternative_sources(
            &candidates,
            Ipv4Addr::new(169, 254, 9, 9),
            Ipv4Addr::new(192, 168, 1, 108),
        );
        assert!(alts.is_empty());
    }

    #[test]
    fn zero_prefix_is_not_a_match() {
        // A /0 matches everything and so proves nothing about locality.
        let candidates = [lif([25, 60, 1, 1], 0)];
        let alts = alternative_sources(
            &candidates,
            Ipv4Addr::new(192, 168, 1, 106),
            Ipv4Addr::new(10, 0, 0, 1),
        );
        assert!(alts.is_empty());
    }

    #[test]
    fn common_prefix_len_ranks_closeness() {
        let dest = Ipv4Addr::new(192, 168, 1, 106);
        let near = common_prefix_len(Ipv4Addr::new(192, 168, 1, 108), dest);
        let far = common_prefix_len(Ipv4Addr::new(192, 168, 243, 92), dest);
        assert!(near > far, "same-segment address must rank closer");
    }

    /// The OS route lookup must agree with the routing table for a loopback
    /// destination, and must not send anything to find out.
    #[test]
    fn os_source_for_loopback_is_loopback() {
        let src = os_source_for("127.0.0.1".parse().unwrap());
        assert_eq!(src, Some("127.0.0.1".parse::<IpAddr>().unwrap()));
    }
}
