/// Synchronous RTSP TCP connection with optional ChaCha20-Poly1305 encryption.
///
/// Plaintext mode: reads/writes raw bytes.
/// Encrypted mode: wraps each write in the ChaCha20 frame and unwraps reads.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use openair_crypto::ChaChaChannel;
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
}

impl RtspConnection {
    pub fn connect(addr: impl ToSocketAddrs + Copy, device_id: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
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
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
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

    fn read_encrypted_response(&mut self) -> io::Result<Vec<u8>> {
        // Read the 2-byte little-endian length prefix, then ciphertext + 16-byte tag.
        let mut len_buf = [0u8; 2];
        self.stream.read_exact(&mut len_buf)?;
        let payload_len = u16::from_le_bytes(len_buf) as usize;
        let total = 2 + payload_len + 16;
        let mut frame = vec![0u8; total];
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
