/// Synchronous RTSP TCP connection with optional ChaCha20-Poly1305 encryption.
///
/// Plaintext mode: reads/writes raw bytes.
/// Encrypted mode: wraps each write in the ChaCha20 frame and unwraps reads.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use openair_crypto::ChaChaChannel;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct RtspConnection {
    stream: TcpStream,
    cseq: u32,
    device_id: String,
    session_id: String,
    pub encrypt: Option<(ChaChaChannel, ChaChaChannel)>, // (write, read)
}

impl RtspConnection {
    pub fn connect(addr: impl ToSocketAddrs, device_id: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(RtspConnection {
            stream,
            cseq: 1,
            device_id: device_id.to_string(),
            session_id: new_session_id(),
            encrypt: None,
        })
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
        req.push_str(&format!("{} rtsp://{}:{}{} RTSP/1.0\r\n",
            method, self.device_id, 7000, path));
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
        let mut headers = Vec::new();
        let mut content_length = 0usize;

        // Read headers until blank line.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                content_length = lower
                    .trim_start_matches("content-length:")
                    .trim()
                    .trim_end()
                    .parse::<usize>()
                    .unwrap_or(0);
            }
            headers.extend_from_slice(line.as_bytes());
        }
        headers.extend_from_slice(b"\r\n");

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        headers.extend_from_slice(&body);
        Ok(headers)
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

/// Extract the HTTP body from a raw response (everything after the blank line).
pub fn extract_body(response: &[u8]) -> &[u8] {
    // Find \r\n\r\n
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
