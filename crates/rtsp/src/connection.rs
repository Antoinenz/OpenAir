/// Synchronous RTSP TCP connection with optional ChaCha20-Poly1305 encryption.
///
/// Plaintext mode: reads/writes raw bytes.
/// Encrypted mode: wraps each write in the ChaCha20 frame and unwraps reads.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use openair_crypto::ChaChaChannel;

use crate::message;
use tracing::debug;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct RtspConnection {
    stream: TcpStream,
    /// Peer address — used to build the RTSP request-URI.
    peer: SocketAddr,
    cseq: u32,
    /// MAC-like device identifier for X-Apple-Device-ID header.
    device_id: String,
    session_id: String,
    pub encrypt: Option<(ChaChaChannel, ChaChaChannel)>, // (write, read)
    /// (events_write, events_read) for the reverse event channel.
    event_keys: Option<([u8; 32], [u8; 32])>,
}

impl RtspConnection {
    pub fn connect(addr: impl ToSocketAddrs + Copy, device_id: &str) -> io::Result<Self> {
        // Bind to the interface that actually reaches the receiver — the OS
        // may otherwise source this from a virtual adapter, and the address we
        // end up with is also what SETPEERS advertises for PTP.
        let dest = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to connect to"))?;
        let stream = openair_core::net::connect_from_best_source(dest)?;
        let peer = stream.peer_addr()?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(RtspConnection {
            stream,
            peer,
            cseq: 1,
            device_id: device_id.to_string(),
            session_id: new_session_id(),
            encrypt: None,
            event_keys: None,
        })
    }

    /// Local IP of this connection (for rtsp:// request URIs).
    pub fn local_ip(&self) -> std::net::IpAddr {
        self.stream
            .local_addr()
            .map(|a| a.ip())
            .unwrap_or_else(|_| std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    }

    /// Remote (receiver) IP.
    pub fn peer_ip(&self) -> std::net::IpAddr {
        self.peer.ip()
    }

    /// Enable encrypted mode after successful pairing.
    pub fn enable_encryption(&mut self, write_key: &[u8; 32], read_key: &[u8; 32]) {
        self.encrypt = Some((
            ChaChaChannel::new(write_key),
            ChaChaChannel::new(read_key),
        ));
    }

    /// Remember the reverse event channel's keys so the caller can decrypt what
    /// the receiver pushes there. Stored rather than used here because the
    /// event channel is a separate socket owned by the client layer.
    pub fn set_event_keys(&mut self, write_key: [u8; 32], read_key: [u8; 32]) {
        self.event_keys = Some((write_key, read_key));
    }

    /// `(events_write, events_read)` if pairing completed, else `None`.
    pub fn event_keys(&self) -> Option<([u8; 32], [u8; 32])> {
        self.event_keys
    }

    /// Send an RTSP request and return the raw response bytes.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: &[u8],
        content_type: Option<&str>,
    ) -> io::Result<Vec<u8>> {
        let cseq = self.cseq;
        self.cseq += 1;

        let mut req = String::new();
        // Use just the path in the request-line. Shairport Sync (and many receivers)
        // match handlers on the bare path; a full rtsp:// URI fails the strcmp.
        req.push_str(&format!("{} {} RTSP/1.0\r\n", method, path));
        req.push_str(&format!("CSeq: {}\r\n", cseq));
        req.push_str("User-Agent: AirPlay/770.8.1\r\n");
        req.push_str("X-Apple-ProtocolVersion: 1\r\n");
        req.push_str(&format!("X-Apple-Device-ID: {}\r\n", self.device_id));
        req.push_str(&format!("X-Apple-Session-ID: {}\r\n", self.session_id));
        for (k, v) in extra_headers {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !body.is_empty() {
            let ct = content_type.unwrap_or("application/pairing+tlv8");
            req.push_str(&format!("Content-Type: {}\r\n", ct));
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        } else {
            req.push_str("Content-Length: 0\r\n");
        }
        req.push_str("\r\n");

        let req_bytes: Vec<u8> = req.into_bytes().into_iter().chain(body.iter().copied()).collect();

        self.write_bytes(&req_bytes)?;
        self.read_response()
    }

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some((write_ch, _)) = &mut self.encrypt {
            let framed = write_ch.encrypt(data)
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.stream.write_all(&framed)
        } else {
            self.stream.write_all(data)
        }
    }

    fn read_response(&mut self) -> io::Result<Vec<u8>> {
        if self.encrypt.is_some() {
            self.read_encrypted_response()
        } else {
            self.read_plain_response()
        }
    }

    fn read_plain_response(&mut self) -> io::Result<Vec<u8>> {
        let mut reader = BufReader::new(&self.stream);
        let mut header_lines: Vec<String> = Vec::new();
        let mut content_length: Option<usize> = None;
        let mut chunked = false;

        // Read headers until blank line.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            let lower = trimmed.to_lowercase();
            if let Some(rest) = lower.strip_prefix("content-length:") {
                content_length = rest.trim().parse::<usize>().ok();
            }
            if lower.contains("transfer-encoding") && lower.contains("chunked") {
                chunked = true;
            }
            header_lines.push(line);
        }

        for h in &header_lines {
            debug!(header = h.trim_end_matches(['\r', '\n']), "rx header");
        }

        // Reconstruct header block for callers that use extract_body / status_code.
        let mut out = header_lines.join("").into_bytes();
        out.extend_from_slice(b"\r\n");

        let body = if chunked {
            read_chunked_body(&mut reader)?
        } else if let Some(len) = content_length {
            let mut body = vec![0u8; len];
            if len > 0 {
                reader.read_exact(&mut body)?;
            }
            body
        } else {
            // No Content-Length and not chunked: receiver sends body then holds
            // the connection open (Shairport Sync / AirTunes/366.0 style).
            // Set a short drain timeout — body arrives immediately after headers;
            // we stop as soon as the server goes quiet.
            self.stream.set_read_timeout(Some(Duration::from_millis(500)))?;
            let mut body = Vec::new();
            let mut chunk = vec![0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut
                           || e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            self.stream.set_read_timeout(Some(READ_TIMEOUT))?;
            body
        };

        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Read one encrypted RTSP response, across as many frames as it takes.
    ///
    /// **A response is not a frame.** The channel carries at most 1024 bytes of
    /// plaintext per frame, and receivers chunk their side at exactly that:
    /// DEVLOG session 19 records an Apple TV sending ~2518 bytes on the event
    /// channel as three frames, each opening `00 04`.
    ///
    /// This used to read a single frame and return it. Anything longer than
    /// 1024 bytes was silently cut to its first frame, and — worse — the rest
    /// stayed in the socket, so the *next* response read the tail of the
    /// previous one and every reply after that was shifted by one. It never
    /// bit because everything operational fits: the largest recorded response
    /// is a 701-byte `GET /info` from Shairport. An Apple TV's is bigger.
    fn read_encrypted_response(&mut self) -> io::Result<Vec<u8>> {
        let mut msg = Vec::new();
        loop {
            msg.extend_from_slice(&self.read_encrypted_frame()?);
            let Some(head) = message::header_block_end(&msg) else {
                continue; // headers still arriving
            };
            match message::declared_body_len(&msg[..head]) {
                Some(len) if msg.len() < head + len => continue,
                // Complete. Trim anything past the declared end rather than
                // hand it back as body: responses are answers to requests we
                // send one at a time, so there should be nothing there, and
                // passing it on would put one message's tail inside another.
                Some(len) => {
                    msg.truncate(head + len);
                    return Ok(msg);
                }
                // No Content-Length. Nothing says how much more is coming, and
                // this stream cannot be un-read, so one frame is all we can
                // honestly claim -- which is what the plaintext reader does
                // with the same situation.
                None => return Ok(msg),
            }
        }
    }

    /// Read and decrypt one frame: `u16 LE length || ciphertext || 16-byte tag`.
    fn read_encrypted_frame(&mut self) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 2];
        self.stream.read_exact(&mut len_buf)?;
        let payload_len = u16::from_le_bytes(len_buf) as usize;
        let mut frame = vec![0u8; 2 + payload_len + 16];
        frame[0] = len_buf[0];
        frame[1] = len_buf[1];
        self.stream.read_exact(&mut frame[2..])?;

        if let Some((_, read_ch)) = &mut self.encrypt {
            read_ch.decrypt(&frame)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
        } else {
            unreachable!()
        }
    }
}

/// Decode an HTTP chunked transfer-encoded body.
fn read_chunked_body(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let chunk_size = usize::from_str_radix(size_line.trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if chunk_size == 0 {
            // Trailing CRLF after last chunk.
            let mut _crlf = String::new();
            let _ = reader.read_line(&mut _crlf);
            break;
        }
        let mut chunk = vec![0u8; chunk_size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        // Consume trailing CRLF after chunk data.
        let mut _crlf = String::new();
        reader.read_line(&mut _crlf)?;
    }
    Ok(body)
}

/// Extract the HTTP body from a raw response (everything after the blank line).
pub fn extract_body(response: &[u8]) -> &[u8] {
    for i in 0..response.len().saturating_sub(3) {
        if &response[i..i + 4] == b"\r\n\r\n" {
            return &response[i + 4..];
        }
    }
    &[]
}

/// Extract the HTTP status code from the first response line.
pub fn status_code(response: &[u8]) -> Option<u16> {
    let line = response.split(|&b| b == b'\r' || b == b'\n').next()?;
    let s = std::str::from_utf8(line).ok()?;
    // "RTSP/1.0 200 OK"
    let code = s.split_whitespace().nth(1)?;
    code.parse().ok()
}

fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        t, t >> 16, 0x4000 | (t >> 12 & 0x0FFF),
        0x8000 | (t >> 10 & 0x3FFF), t as u64 * 0x1234567)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openair_crypto::chacha::MAX_FRAME_PLAINTEXT;
    use std::net::TcpListener;

    /// Stand in for a receiver: accept one connection, swallow whatever the
    /// client sends, then write `response` through an encrypted channel keyed
    /// the way the client expects to read.
    fn fake_receiver(response: Vec<u8>) -> (SocketAddr, std::thread::JoinHandle<()>, [u8; 32]) {
        let key = [0x5Au8; 32];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // The request arrives first and we do not care what it says; read
            // one frame's worth so the client's write completes.
            let mut scratch = [0u8; 4096];
            let _ = sock.read(&mut scratch);
            let mut tx = ChaChaChannel::new(&key);
            // `encrypt` chunks at MAX_FRAME_PLAINTEXT on its own, which is
            // exactly what a real receiver does.
            let framed = tx.encrypt(&response).unwrap();
            sock.write_all(&framed).unwrap();
            // Hold the connection open so the client is reading from a live
            // socket rather than racing a close.
            std::thread::sleep(Duration::from_millis(300));
        });
        (addr, handle, key)
    }

    /// A response longer than one frame comes back whole.
    ///
    /// The regression: this read exactly one frame, so anything past 1024
    /// bytes of plaintext was cut off and — the part that would have been hard
    /// to diagnose — left in the socket, shifting every later response by one.
    #[test]
    fn a_response_spanning_several_frames_is_reassembled() {
        let body = vec![b'x'; MAX_FRAME_PLAINTEXT * 3 + 17];
        let mut response =
            format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n", body.len())
                .into_bytes();
        response.extend_from_slice(&body);
        assert!(
            response.len() > MAX_FRAME_PLAINTEXT,
            "the test is pointless if it fits in one frame"
        );

        let (addr, server, key) = fake_receiver(response.clone());
        let mut conn = RtspConnection::connect(addr, "AA:BB:CC:DD:EE:FF").unwrap();
        // Only the read direction is exercised; the write key is unused by the
        // receiver stub above.
        conn.enable_encryption(&[0x11u8; 32], &key);

        let got = conn.request("GET", "/info", &[], &[], None).unwrap();
        assert_eq!(got.len(), response.len(), "truncated to its first frame");
        assert_eq!(extract_body(&got), &body[..], "body came back mangled");
        server.join().unwrap();
    }

    /// A response that fits in one frame still works, unchanged.
    #[test]
    fn a_single_frame_response_is_unaffected() {
        let response = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\nokay".to_vec();
        let (addr, server, key) = fake_receiver(response.clone());
        let mut conn = RtspConnection::connect(addr, "AA:BB:CC:DD:EE:FF").unwrap();
        conn.enable_encryption(&[0x11u8; 32], &key);

        let got = conn.request("GET", "/info", &[], &[], None).unwrap();
        assert_eq!(status_code(&got), Some(200));
        assert_eq!(extract_body(&got), b"okay");
        server.join().unwrap();
    }
}
