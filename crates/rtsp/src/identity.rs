//! The sender identity we present to receivers.
//!
//! AirPlay senders announce a `deviceID`/`macAddress` in SETUP. OpenAir has
//! always sent a fixed placeholder, which means every run looks to a receiver
//! like the *same* sender device coming back.
//!
//! That is under suspicion: an Apple TV sets `ReceiverSessionIsActive`
//! (`statusFlags` bit `0x100000`) and displays now-playing metadata on the
//! first session after a reboot, then never again until it is rebooted —
//! regardless of session length, teardown, or anything else we send. A stale
//! per-sender record on the receiver would explain that exactly.
//!
//! [`randomise`] exists to test that, behind `--random-sender-id`. It is
//! deliberately **not** the default: changing the identity we present is a
//! protocol-visible change, and it should be earned by evidence rather than
//! adopted on a hunch.
//!
//! ## Why one identity per process, not per session
//!
//! A multi-room group must look like a single sender to every receiver in it.
//! Randomising per `StreamSession` would present a different device to each
//! member of the group, which is precisely what grouping keys off. So the
//! identity is generated once per run and shared.

use std::sync::OnceLock;

use rand::RngCore;

/// The placeholder every OpenAir build has sent to date.
pub const DEFAULT_SENDER_ID: &str = "AA:BB:CC:DD:EE:FF";

static SENDER_ID: OnceLock<String> = OnceLock::new();

/// The sender identity for this run.
pub fn sender_id() -> &'static str {
    SENDER_ID.get().map(String::as_str).unwrap_or(DEFAULT_SENDER_ID)
}

/// Present a freshly generated identity for this run.
///
/// Call once, before the first SETUP. Later calls do nothing: the identity has
/// already been shown to a receiver by then, and changing it mid-run would make
/// two receivers in one group disagree about who is sending.
pub fn randomise() -> &'static str {
    SENDER_ID.get_or_init(random_mac)
}

/// The device this sender claims to be, announced in the SETUP plist.
///
/// All fields are `&'static str` so a profile can be a `const` and live in
/// a `OnceLock` without allocation or locking on the read path.
///
/// Receivers make presentation decisions from these fields, so what we claim is
/// not cosmetic.
pub struct SenderProfile {
    pub model: &'static str,
    pub name: &'static str,
    pub os_name: &'static str,
    pub os_version: &'static str,
    /// Absent for OpenAir's own profile; real Apple senders always send one.
    pub os_build_version: Option<&'static str>,
}

/// What we are: honest, and unlike anything a receiver has seen before.
pub const OPENAIR: SenderProfile = SenderProfile {
    model: "OpenAir1,1",
    name: "OpenAir",
    os_name: "Windows",
    os_version: "10",
    os_build_version: None,
};

/// An iPhone, copied field-for-field from pyatv's AirPlay 2 sender — a known
/// working third-party implementation.
///
/// `name` is deliberately *not* copied. It is a free-text label (a real iPhone
/// sends "Someone's iPhone", pyatv sends "pyatv"), so it cannot be what a
/// receiver keys behaviour on, and showing "pyatv" on someone's television
/// would be baffling. Every field that describes the device *class* is copied
/// exactly.
pub const IPHONE: SenderProfile = SenderProfile {
    model: "iPhone14,3",
    name: "iPhone",
    os_name: "iPhone OS",
    os_version: "16.5",
    os_build_version: Some("20F66"),
};

static PROFILE: OnceLock<&'static SenderProfile> = OnceLock::new();

/// The profile for this run.
pub fn profile() -> &'static SenderProfile {
    PROFILE.get().copied().unwrap_or(&OPENAIR)
}

/// Claim to be an iPhone for this run.
///
/// Diagnostic, behind `--impersonate-iphone`. The open question it exists to
/// answer: an Apple TV shows no AirPlay UI at all for any session after the
/// first one following a reboot, and every difference between a working and a
/// failing run of *ours* has been ruled out — the RTSP exchange is byte
/// identical. What has not been ruled out is that a receiver treats an unknown
/// sender model differently from an iPhone.
///
/// Idempotent within a run, and for the same reason as [`randomise`]: every
/// receiver in a group must be told the same thing.
pub fn impersonate_iphone() -> &'static SenderProfile {
    PROFILE.get_or_init(|| &IPHONE)
}

/// A random locally-administered unicast MAC.
///
/// Bit 1 of the first octet set marks it locally administered, and bit 0 clear
/// marks it unicast — so it cannot collide with a real vendor-assigned address
/// or be mistaken for a multicast one.
fn random_mac() -> String {
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[0] = (bytes[0] | 0b0000_0010) & 0b1111_1110;
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_random_mac_is_locally_administered_and_unicast() {
        for _ in 0..100 {
            let mac = random_mac();
            let first = u8::from_str_radix(&mac[..2], 16).unwrap();
            assert_eq!(first & 0b10, 0b10, "{mac} is not locally administered");
            assert_eq!(first & 0b01, 0, "{mac} is multicast");
        }
    }

    #[test]
    fn a_random_mac_is_well_formed() {
        let mac = random_mac();
        let parts: Vec<&str> = mac.split(':').collect();
        assert_eq!(parts.len(), 6, "{mac}");
        for p in parts {
            assert_eq!(p.len(), 2, "{mac}");
            assert!(
                p.chars().all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()),
                "{mac} should be uppercase hex"
            );
        }
    }

    #[test]
    fn random_macs_differ() {
        // The whole point is that two runs do not collide.
        let a = random_mac();
        let b = random_mac();
        assert_ne!(a, b);
    }

    #[test]
    fn the_iphone_profile_matches_pyatv_field_for_field() {
        // Copied from pyatv's AirPlay 2 sender, which is the known-working
        // third-party implementation this test exists to imitate. If these
        // drift, the experiment stops being the one we think we are running.
        assert_eq!(IPHONE.model, "iPhone14,3");
        assert_eq!(IPHONE.os_name, "iPhone OS");
        assert_eq!(IPHONE.os_version, "16.5");
        assert_eq!(IPHONE.os_build_version, Some("20F66"));
    }

    #[test]
    fn openair_sends_no_build_version_and_the_iphone_does() {
        // Real Apple senders always send one; we never have. That asymmetry is
        // part of what is being tested, so it is pinned rather than incidental.
        assert!(OPENAIR.os_build_version.is_none());
        assert!(IPHONE.os_build_version.is_some());
    }

    #[test]
    fn the_profiles_describe_different_devices() {
        assert_ne!(OPENAIR.model, IPHONE.model);
        assert_ne!(OPENAIR.os_name, IPHONE.os_name);
    }

    /// The only test that touches the process-wide identity.
    ///
    /// Kept as one test rather than several because `OnceLock` is set once per
    /// process and Rust runs tests in parallel: two tests each asserting on the
    /// global would race on who set it first.
    #[test]
    fn the_identity_defaults_then_sticks_once_randomised() {
        assert_eq!(sender_id(), DEFAULT_SENDER_ID, "unchanged unless asked");

        let first = randomise().to_string();
        assert_ne!(first, DEFAULT_SENDER_ID);
        assert_eq!(sender_id(), first, "the run keeps one identity");

        // A second call must not hand a different identity to a receiver that
        // has already seen the first — that would split a multi-room group.
        assert_eq!(randomise(), first, "randomise is idempotent within a run");

        // The profile global, folded in here for the same reason: OnceLock is
        // set once per process and tests run in parallel, so two tests each
        // asserting on it would race on who set it first.
        assert_eq!(profile().model, OPENAIR.model, "unchanged unless asked");
        assert_eq!(impersonate_iphone().model, IPHONE.model);
        assert_eq!(profile().model, IPHONE.model, "the run keeps one profile");
        assert_eq!(
            impersonate_iphone().model,
            IPHONE.model,
            "idempotent within a run, like the sender id"
        );
    }
}
