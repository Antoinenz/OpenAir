//! Marking our audio packets as the real-time traffic they are.
//!
//! # Why this exists
//!
//! Wi-Fi access points do not treat all packets alike. 802.11e/WMM sorts
//! traffic into four access categories, and a frame marked as voice contends
//! for the air sooner than one marked best-effort. On a congested network that
//! difference is the difference between a stream that holds and one that
//! stutters -- which is precisely the situation the user is in when they
//! notice.
//!
//! The marking is a DSCP value in the IP header. **EF** (Expedited Forwarding,
//! DSCP 46) is the standard codepoint for low-latency real-time media, and is
//! what access points map to the voice category.
//!
//! # Windows will probably ignore this
//!
//! Since XP SP2, Windows silently refuses to let an application set the TOS
//! byte through `setsockopt`. The call *succeeds* -- there is no error to
//! detect -- and the packets go out marked zero anyway. The supported routes
//! are the qWAVE API or a machine-wide QoS policy.
//!
//! So this sets the option, and then goes and *reads* whether the machine is
//! configured to honour it, because a QoS feature that quietly does nothing is
//! worse than no QoS feature at all: it moves the problem from "unsolved" to
//! "believed solved".
use std::net::UdpSocket;

use socket2::SockRef;

/// Expedited Forwarding: DSCP 46, in the top six bits of the TOS byte.
///
/// `46 << 2 == 0xB8`. The low two bits are ECN and are left clear.
pub const DSCP_EF: u32 = 46 << 2;

/// What happened when we asked for EF marking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marking {
    /// The socket option was set and the OS is expected to honour it.
    Applied,
    /// The option was accepted, but this machine is configured to ignore it.
    ///
    /// Carries the reason so it can be shown to someone who wants their
    /// packets prioritised and is wondering why nothing changed.
    Ignored(&'static str),
    /// The option was refused outright.
    Failed(String),
}

impl Marking {
    /// Whether the packets will actually carry the marking.
    pub fn effective(&self) -> bool {
        matches!(self, Marking::Applied)
    }
}

/// Ask for EF marking on an already-connected UDP socket.
///
/// Never fails outward. A stream that refused to start because it could not
/// set a QoS hint would be trading the whole feature for a nicety.
pub fn mark_ef(socket: &UdpSocket) -> Marking {
    let sock = SockRef::from(socket);
    if let Err(e) = sock.set_tos(DSCP_EF) {
        let marking = Marking::Failed(e.to_string());
        tracing::debug!("could not request EF marking: {e}");
        return marking;
    }
    let marking = honoured();
    match &marking {
        Marking::Applied => tracing::debug!("audio packets marked DSCP EF"),
        Marking::Ignored(why) => tracing::debug!("DSCP EF requested but {why}"),
        Marking::Failed(e) => tracing::debug!("could not request EF marking: {e}"),
    }
    marking
}

/// Whether this machine honours an application's TOS byte.
#[cfg(windows)]
fn honoured() -> Marking {
    // HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters
    //   DisableUserTOSSetting (DWORD)
    //     absent or 1 -> the TOS byte we set is stripped (the default)
    //     0           -> our marking is passed through
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
        REG_VALUE_TYPE,
    };

    const DEFAULT: Marking = Marking::Ignored(
        "Windows strips it unless the DisableUserTOSSetting value under Tcpip/Parameters is 0",
    );

    let mut key = HKEY::default();
    // SAFETY: both wide strings are static and null-terminated, and `key` is a
    // valid out-pointer for the duration of the call.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters"),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_err() {
        return DEFAULT;
    }

    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    // SAFETY: `key` is open, and the out-pointers address a live u32 and its
    // matching size for the duration of the call.
    let read = unsafe {
        RegQueryValueExW(
            key,
            w!("DisableUserTOSSetting"),
            None,
            Some(&mut kind),
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut size),
        )
    };
    // SAFETY: `key` was opened above and is not used again.
    unsafe {
        let _ = RegCloseKey(key);
    }

    if read.is_err() {
        // Absent means the default, which is to strip it.
        return DEFAULT;
    }
    if value == 0 {
        Marking::Applied
    } else {
        DEFAULT
    }
}

#[cfg(not(windows))]
fn honoured() -> Marking {
    // Linux and macOS pass an application's TOS byte through as set.
    Marking::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ef_is_the_standard_codepoint() {
        // DSCP 46 occupies the top six bits; the low two are ECN and stay
        // clear. Getting this wrong marks the traffic as something else
        // entirely, which is worse than not marking it.
        assert_eq!(DSCP_EF, 0xB8);
        assert_eq!(DSCP_EF >> 2, 46);
        assert_eq!(DSCP_EF & 0b11, 0, "ECN bits must be left alone");
    }

    #[test]
    fn marking_a_real_socket_reports_what_happened() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        match mark_ef(&socket) {
            // On Windows the honest answer is almost always Ignored, and that
            // is the point of reporting it rather than assuming.
            Marking::Applied | Marking::Ignored(_) => {}
            Marking::Failed(why) => assert!(!why.is_empty(), "a refusal must say why"),
        }
    }

    #[test]
    fn only_applied_counts_as_effective() {
        // The distinction this whole module exists for: a QoS feature that
        // quietly does nothing must not report success.
        assert!(Marking::Applied.effective());
        assert!(!Marking::Ignored("stripped").effective());
        assert!(!Marking::Failed("refused".into()).effective());
    }
}
