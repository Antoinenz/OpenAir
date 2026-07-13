//! Streaming session: SETUP (two-phase), RECORD, SET_PARAMETER, feedback,
//! TEARDOWN — everything after pairing, over the encrypted RTSP channel.
//!
//! Bodies are binary plists (`application/x-apple-binary-plist`).
//! Flow and field values verified against pyatv's AirPlayV2 (NTP realtime).
use std::net::SocketAddr;

use rand::RngCore;
use tracing::{debug, info};

use crate::connection::{self, RtspConnection};
use crate::session::{pair, SessionError};

/// Ports negotiated with the receiver via SETUP.
#[derive(Debug, Clone, Copy, Default)]
pub struct NegotiatedPorts {
    pub event_port: u16,
    pub timing_port: u16,
    pub data_port: u16,
    pub control_port: u16,
}

/// Timing protocol for SETUP phase 1.
#[derive(Debug, Clone, Copy)]
pub enum TimingConfig {
    /// PTP master run by the sender (Shairport Sync, Apple TV, HomePod).
    Ptp,
    /// NTP-style timing on the given sender UDP port (AirPort Express era).
    Ntp { port: u16 },
}

/// Audio stream parameters for SETUP phase 2.
#[derive(Debug, Clone, Copy)]
pub enum StreamFormat {
    /// ALAC/44100/16/2 realtime (ct=2, audioFormat 0x40000)
    AlacRealtime,
    /// Raw PCM realtime (ct=1, audioFormat 0x800) — pyatv-style fallback
    PcmRealtime,
}

/// A paired, encrypted RTSP session ready for stream negotiation.
pub struct StreamSession {
    conn: RtspConnection,
    uri: String,
    /// Random u32 also used as SSRC and streamConnectionID.
    pub session_id: u32,
    dacp_id: String,
    active_remote: u32,
    /// AEAD key for RTP audio (we generate it, receiver gets it in SETUP 2).
    pub shk: [u8; 32],
    pub ports: NegotiatedPorts,
}

impl StreamSession {
    /// Connect + Transient-pair + encrypt. `device_id` from discovery TXT.
    pub fn connect(addr: SocketAddr, device_id: &str) -> Result<Self, SessionError> {
        let conn = pair(addr, device_id)?;
        let local_ip = conn.local_ip();
        let mut rng = rand::thread_rng();
        let session_id = rng.next_u32();
        let mut shk = [0u8; 32];
        rng.fill_bytes(&mut shk);
        Ok(StreamSession {
            uri: format!("rtsp://{}/{}", local_ip, session_id),
            session_id,
            dacp_id: format!("{:016X}", rng.next_u64()),
            active_remote: rng.next_u32(),
            shk,
            ports: NegotiatedPorts::default(),
            conn,
        })
    }

    /// SETUP phase 1: timing + session metadata → eventPort/timingPort.
    ///
    /// `TimingConfig::Ptp` — required by Shairport Sync and HomePod/Apple TV;
    /// the sender must run a PTP master (see `openair-timing::PtpMaster`).
    /// `TimingConfig::Ntp { port }` — AirPort Express-era receivers only;
    /// Shairport Sync answers "can not handle NTP streams".
    pub fn setup_timing(&mut self, timing: TimingConfig) -> Result<(), SessionError> {
        let session_uuid = uuid_v4_ish();
        let mut dict = plist::Dictionary::new();
        dict.insert("deviceID".into(), "AA:BB:CC:DD:EE:FF".into());
        dict.insert("macAddress".into(), "AA:BB:CC:DD:EE:FF".into());
        dict.insert("sessionUUID".into(), session_uuid.clone().into());
        match timing {
            TimingConfig::Ptp => {
                dict.insert("timingProtocol".into(), "PTP".into());
                dict.insert("groupUUID".into(), session_uuid.into());
                let local_ip = self.conn.local_ip().to_string();
                let mut peer_info = plist::Dictionary::new();
                peer_info.insert(
                    "Addresses".into(),
                    plist::Value::Array(vec![local_ip.clone().into()]),
                );
                peer_info.insert("ID".into(), local_ip.into());
                dict.insert("timingPeerInfo".into(), plist::Value::Dictionary(peer_info));
            }
            TimingConfig::Ntp { port } => {
                dict.insert("timingProtocol".into(), "NTP".into());
                dict.insert("timingPort".into(), (port as u64).into());
            }
        }
        dict.insert("isMultiSelectAirPlay".into(), true.into());
        dict.insert("groupContainsGroupLeader".into(), false.into());
        dict.insert("model".into(), "OpenAir1,1".into());
        dict.insert("name".into(), "OpenAir".into());
        dict.insert("osName".into(), "Windows".into());
        dict.insert("osVersion".into(), "10".into());
        dict.insert("senderSupportsRelay".into(), false.into());
        dict.insert("sourceVersion".into(), "690.7.1".into());
        dict.insert("statsCollectionEnabled".into(), false.into());

        info!(?timing, "SETUP phase 1 (timing)");
        let resp = self.request_plist("SETUP", None, plist::Value::Dictionary(dict))?;
        self.ports.event_port = get_port(&resp, "eventPort")?;
        self.ports.timing_port = get_port(&resp, "timingPort").unwrap_or(0);
        info!(event_port = self.ports.event_port, timing_port = self.ports.timing_port,
              "SETUP 1 ok");
        Ok(())
    }

    /// SETRATEANCHORTIME with only a `rate` (1 = play, 0 = pause).
    ///
    /// For realtime PTP streams the anchor itself travels on the control
    /// channel (type-215 packets); sending anchor fields here as well creates
    /// two competing anchor sources and constant receiver resyncs.
    pub fn set_rate(&mut self, rate: u64) -> Result<(), SessionError> {
        let mut dict = plist::Dictionary::new();
        dict.insert("rate".into(), rate.into());

        info!(rate, "SETRATEANCHORTIME (rate only)");
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(dict))
            .map_err(|_| SessionError::PlistEncode)?;
        let raw = self.conn.request(
            "SETRATEANCHORTIME",
            &self.uri.clone(),
            &[
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
            ],
            &buf,
            Some("application/x-apple-binary-plist"),
        )?;
        check_ok(&raw)
    }

    /// SETUP phase 2: audio stream definition → dataPort/controlPort.
    pub fn setup_stream(
        &mut self,
        format: StreamFormat,
        control_port: u16,
    ) -> Result<(), SessionError> {
        let (ct, audio_format): (u64, u64) = match format {
            StreamFormat::AlacRealtime => (2, 0x40000),
            StreamFormat::PcmRealtime => (1, 0x800),
        };
        let mut stream = plist::Dictionary::new();
        stream.insert("type".into(), 96u64.into());
        stream.insert("ct".into(), ct.into());
        stream.insert("audioFormat".into(), audio_format.into());
        stream.insert("audioMode".into(), "default".into());
        stream.insert("spf".into(), 352u64.into());
        stream.insert("sr".into(), 44100u64.into());
        stream.insert("latencyMin".into(), 11025u64.into());
        stream.insert("latencyMax".into(), 88200u64.into());
        stream.insert("shk".into(), plist::Value::Data(self.shk.to_vec()));
        stream.insert("controlPort".into(), (control_port as u64).into());
        stream.insert("isMedia".into(), true.into());
        stream.insert("supportsDynamicStreamID".into(), false.into());
        stream.insert("streamConnectionID".into(), (self.session_id as u64).into());

        let mut dict = plist::Dictionary::new();
        dict.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(stream)]),
        );

        info!(?format, "SETUP phase 2 (audio stream)");
        let resp = self.request_plist("SETUP", None, plist::Value::Dictionary(dict))?;
        let streams = resp
            .as_dictionary()
            .and_then(|d| d.get("streams"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_dictionary())
            .ok_or(SessionError::MissingPlistField("streams[0]"))?;
        self.ports.data_port = dict_port(streams, "dataPort")?;
        self.ports.control_port = dict_port(streams, "controlPort")?;
        info!(data_port = self.ports.data_port, control_port = self.ports.control_port,
              "SETUP 2 ok");
        Ok(())
    }

    /// RECORD — start the session. `seq`/`rtptime` describe the first packet.
    pub fn record(&mut self, seq: u16, rtptime: u32) -> Result<(), SessionError> {
        info!("RECORD");
        let rtp_info = format!("seq={};rtptime={}", seq, rtptime);
        let raw = self.conn.request(
            "RECORD",
            &self.uri.clone(),
            &[
                ("Range", "npt=0-"),
                ("Session", "1"),
                ("RTP-Info", &rtp_info),
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
                ("X-Apple-ProtocolVersion", "1"),
            ],
            &[],
            None,
        )?;
        check_ok(&raw)
    }

    /// SET_PARAMETER volume (dBFS: 0 = full, -30 = quiet, -144 = mute).
    pub fn set_volume(&mut self, db: f32) -> Result<(), SessionError> {
        let body = format!("volume: {:.6}\r\n", db);
        let raw = self.conn.request(
            "SET_PARAMETER",
            &self.uri.clone(),
            &[
                ("Session", "1"),
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
            ],
            body.as_bytes(),
            Some("text/parameters"),
        )?;
        check_ok(&raw)
    }

    /// POST /feedback keepalive (send every ~2s while streaming).
    pub fn feedback(&mut self) -> Result<(), SessionError> {
        let raw = self.conn.request("POST", "/feedback", &[], &[], None)?;
        check_ok(&raw)
    }

    /// TEARDOWN — end the session.
    ///
    /// Must carry a binary-plist body: an empty dict closes the whole
    /// connection (a dict with a "streams" array would close just that
    /// stream). Shairport Sync replies 451 "missing plist!" to a bodyless
    /// TEARDOWN.
    pub fn teardown(&mut self) -> Result<(), SessionError> {
        info!("TEARDOWN");
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(plist::Dictionary::new()))
            .map_err(|_| SessionError::PlistEncode)?;
        let raw = self.conn.request(
            "TEARDOWN",
            &self.uri.clone(),
            &[("Session", "1")],
            &buf,
            Some("application/x-apple-binary-plist"),
        )?;
        check_ok(&raw)
    }

    /// Remote (receiver) IP for the UDP sockets.
    pub fn peer_ip(&self) -> std::net::IpAddr {
        self.conn.peer_ip()
    }

    fn request_plist(
        &mut self,
        method: &str,
        path: Option<&str>,
        body: plist::Value,
    ) -> Result<plist::Value, SessionError> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &body).map_err(|_| SessionError::PlistEncode)?;
        let uri = path.map(str::to_string).unwrap_or_else(|| self.uri.clone());
        let raw = self.conn.request(
            method,
            &uri,
            &[
                ("DACP-ID", &self.dacp_id.clone()),
                ("Active-Remote", &self.active_remote.to_string()),
                ("Client-Instance", &self.dacp_id.clone()),
                ("X-Apple-ProtocolVersion", "1"),
            ],
            &buf,
            Some("application/x-apple-binary-plist"),
        )?;
        check_ok(&raw)?;
        let body = connection::extract_body(&raw);
        debug!(bytes = body.len(), "plist response body");
        plist::from_bytes(body).map_err(|_| SessionError::PlistDecode)
    }
}

fn check_ok(raw: &[u8]) -> Result<(), SessionError> {
    match connection::status_code(raw) {
        Some(200) => Ok(()),
        Some(code) => Err(SessionError::Http(code)),
        None => Err(SessionError::EmptyResponse),
    }
}

fn get_port(resp: &plist::Value, key: &'static str) -> Result<u16, SessionError> {
    resp.as_dictionary()
        .and_then(|d| dict_port(d, key).ok())
        .ok_or(SessionError::MissingPlistField(key))
}

fn dict_port(d: &plist::Dictionary, key: &'static str) -> Result<u16, SessionError> {
    d.get(key)
        .and_then(|v| v.as_unsigned_integer())
        .map(|v| v as u16)
        .ok_or(SessionError::MissingPlistField(key))
}

/// RFC-4122-shaped random UUID string (uppercase, like Apple senders send).
fn uuid_v4_ish() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
