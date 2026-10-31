//! WebSocket-to-TCP tunnel.
//!
//! Connects to the WebSocket relay (raw RDP bytes in binary messages, RFC 6455), then opens a
//! loopback port for FreeRDP. The port accepts a single connection; bytes are copied both ways by
//! two threads until either side closes.
//!
//! For `wss://` the TLS session is driven by hand (`rustls::ClientConnection`) so that the reading
//! thread can block on the socket without holding the lock the writing thread needs.

use std::io::{self, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use anyhow::{bail, Context as _, Result};
use ring::rand::{SecureRandom as _, SystemRandom};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConnection, DigitallySignedStruct, SignatureScheme};

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// Largest WebSocket frame accepted from the relay.
const MAX_FRAME: u64 = 16 * 1024 * 1024;

/// Connects to `url` and returns the loopback port to give FreeRDP.
pub fn start(url: &str, ws_fingerprint: Option<&str>) -> Result<u16> {
    let url = WsUrl::parse(url)?;
    // The URL carries the access ticket: log the host only.
    tracing::info!(host = %url.host, port = url.port, tls = url.tls, "connecting to WebSocket relay");

    let wire = Wire::connect(&url, ws_fingerprint)?;
    let leftover = handshake(&wire, &url)?;

    let listener = TcpListener::bind(("127.0.0.1", 0)).context("cannot open loopback port")?;
    let port = listener.local_addr()?.port();

    thread::spawn(move || {
        if let Err(error) = serve(listener, wire, leftover) {
            tracing::error!("tunnel: {error:#}");
        }
    });

    Ok(port)
}

fn serve(listener: TcpListener, wire: Wire, leftover: Vec<u8>) -> Result<()> {
    let (local, _) = listener.accept().context("loopback accept")?;
    // One connection only.
    drop(listener);
    local.set_nodelay(true)?;
    tracing::info!("FreeRDP connected to the tunnel");

    let upstream = wire.try_clone()?;
    let local_reader = local.try_clone()?;
    thread::spawn(move || {
        if let Err(error) = local_to_relay(local_reader, &upstream) {
            tracing::warn!("tunnel upstream: {error:#}");
        }
        // Tell the relay we are done; it may already be gone.
        let _ = upstream.send(&encode_frame(OPCODE_CLOSE, &[]));
    });

    let result = relay_to_local(&wire, leftover, &local);
    let _ = local.shutdown(Shutdown::Both);
    tracing::info!("tunnel closed");
    result
}

fn local_to_relay(mut local: TcpStream, wire: &Wire) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = local.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        wire.send(&encode_frame(OPCODE_BINARY, &buf[..n]))?;
    }
}

fn relay_to_local(wire: &Wire, mut buf: Vec<u8>, mut local: &TcpStream) -> Result<()> {
    loop {
        while let Some(frame) = decode_frame(&mut buf)? {
            match frame.opcode {
                OPCODE_BINARY | OPCODE_TEXT | OPCODE_CONTINUATION => local.write_all(&frame.payload)?,
                OPCODE_PING => wire.send(&encode_frame(OPCODE_PONG, &frame.payload))?,
                OPCODE_PONG => {}
                OPCODE_CLOSE => return Ok(()),
                other => bail!("unexpected WebSocket opcode {other:#x}"),
            }
        }

        if !wire.recv(&mut buf)? {
            return Ok(());
        }
    }
}

// ── URL ──────────────────────────────────────────────────────────────────────

struct WsUrl {
    tls: bool,
    /// `host[:port]` as written, for the `Host` header.
    authority: String,
    host: String,
    port: u16,
    /// Path and query.
    resource: String,
}

impl WsUrl {
    fn parse(url: &str) -> Result<Self> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("wss://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("ws://") {
            (false, rest)
        } else {
            bail!("the WebSocket URL must start with ws:// or wss://");
        };

        let (authority, resource) = match rest.find(['/', '?']) {
            Some(i) if rest[i..].starts_with('?') => (&rest[..i], format!("/{}", &rest[i..])),
            Some(i) => (&rest[..i], rest[i..].to_owned()),
            None => (rest, "/".to_owned()),
        };

        let (host, port) = match authority.strip_prefix('[') {
            Some(rest) => {
                let (host, after) = rest.split_once(']').context("unterminated IPv6 address in URL")?;
                (host, after.strip_prefix(':'))
            }
            None => match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            },
        };
        if host.is_empty() {
            bail!("the WebSocket URL has no host");
        }

        let port = match port {
            Some(port) => port.parse().context("invalid port in the WebSocket URL")?,
            None if tls => 443,
            None => 80,
        };

        Ok(Self {
            tls,
            authority: authority.to_owned(),
            host: host.to_owned(),
            port,
            resource,
        })
    }
}

// ── Transport (TCP or TLS) ───────────────────────────────────────────────────

/// One end of the connection to the relay; clones share it, so one thread can send while another
/// receives.
enum Wire {
    Plain {
        socket: TcpStream,
        /// Keeps frames sent from different threads whole.
        send_lock: Arc<Mutex<()>>,
    },
    Tls {
        socket: TcpStream,
        session: Arc<Mutex<ClientConnection>>,
    },
}

impl Wire {
    fn connect(url: &WsUrl, ws_fingerprint: Option<&str>) -> Result<Self> {
        let socket = TcpStream::connect((url.host.as_str(), url.port))
            .with_context(|| format!("cannot connect to {}:{}", url.host, url.port))?;
        socket.set_nodelay(true)?;

        if !url.tls {
            return Ok(Self::Plain {
                socket,
                send_lock: Arc::default(),
            });
        }

        let fingerprint = ws_fingerprint.context("a wss:// URL needs `ws_fingerprint`")?;
        let expected = decode_hex(&crate::normalize_fingerprint(fingerprint)).context("invalid `ws_fingerprint`")?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(FingerprintVerifier { expected, provider }))
            .with_no_client_auth();

        let server_name = ServerName::try_from(url.host.clone()).context("invalid host in the WebSocket URL")?;
        let mut session = ClientConnection::new(Arc::new(config), server_name)?;

        let mut io = &socket;
        while session.is_handshaking() {
            session.complete_io(&mut io).context("TLS handshake with the WebSocket relay")?;
        }

        Ok(Self::Tls {
            socket,
            session: Arc::new(Mutex::new(session)),
        })
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Self::Plain { socket, send_lock } => Self::Plain {
                socket: socket.try_clone()?,
                send_lock: Arc::clone(send_lock),
            },
            Self::Tls { socket, session } => Self::Tls {
                socket: socket.try_clone()?,
                session: Arc::clone(session),
            },
        })
    }

    fn send(&self, data: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain { socket, send_lock } => {
                let _guard = send_lock.lock().unwrap_or_else(PoisonError::into_inner);
                (&*socket).write_all(data)
            }
            Self::Tls { socket, session } => {
                let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
                session.writer().write_all(data)?;
                flush_tls(&mut session, socket)
            }
        }
    }

    /// Appends received bytes to `buf`; returns `false` at the end of the stream.
    fn recv(&self, buf: &mut Vec<u8>) -> io::Result<bool> {
        let mut chunk = [0u8; 16 * 1024];
        // Blocks without holding the session lock.
        let n = match self {
            Self::Plain { socket, .. } | Self::Tls { socket, .. } => (&*socket).read(&mut chunk)?,
        };
        if n == 0 {
            return Ok(false);
        }

        let (socket, session) = match self {
            Self::Plain { .. } => {
                buf.extend_from_slice(&chunk[..n]);
                return Ok(true);
            }
            Self::Tls { socket, session } => (socket, session),
        };

        let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
        let mut received = &chunk[..n];
        while !received.is_empty() {
            session.read_tls(&mut received)?;
            session
                .process_new_packets()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }

        let mut open = true;
        loop {
            match session.reader().read(&mut chunk) {
                // close_notify
                Ok(0) => {
                    open = false;
                    break;
                }
                Ok(m) => buf.extend_from_slice(&chunk[..m]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }

        // Alerts or key updates produced while reading.
        flush_tls(&mut session, socket)?;
        Ok(open)
    }
}

fn flush_tls(session: &mut ClientConnection, socket: &TcpStream) -> io::Result<()> {
    let mut io = socket;
    while session.wants_write() {
        session.write_tls(&mut io)?;
    }
    Ok(())
}

/// Accepts exactly the certificate whose SHA-256 is `expected`; the relay usually has a
/// self-signed certificate, so the chain is not checked.
#[derive(Debug)]
struct FingerprintVerifier {
    expected: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if digest.as_ref() == self.expected.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the WebSocket server certificate does not match `ws_fingerprint`".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

// ── WebSocket (RFC 6455) ─────────────────────────────────────────────────────

/// Sends the opening handshake and checks the answer (RFC 6455 4.1); returns the bytes received
/// after it.
fn handshake(wire: &Wire, url: &WsUrl) -> Result<Vec<u8>> {
    let key = encode_base64(&random_bytes::<16>()?);
    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: binary\r\n\
         \r\n",
        url.resource, url.authority,
    );
    wire.send(request.as_bytes())?;

    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 * 1024 {
            bail!("WebSocket handshake response too long");
        }
        if !wire.recv(&mut buf)? {
            bail!("the WebSocket relay closed the connection during the handshake");
        }
    };

    let head = String::from_utf8_lossy(&buf[..end]);
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("101") {
        bail!("the WebSocket relay refused the upgrade: {status}");
    }

    let accept = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, value)| value.trim().to_owned())
        .context("WebSocket handshake response without Sec-WebSocket-Accept")?;

    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    if accept != encode_base64(digest.as_ref()) {
        bail!("invalid Sec-WebSocket-Accept in the WebSocket handshake response");
    }

    tracing::info!("WebSocket connected");
    Ok(buf.split_off(end))
}

struct Frame {
    opcode: u8,
    payload: Vec<u8>,
}

/// Removes the first complete frame from `buf` (RFC 6455 5.2).
fn decode_frame(buf: &mut Vec<u8>) -> Result<Option<Frame>> {
    if buf.len() < 2 {
        return Ok(None);
    }

    let opcode = buf[0] & 0x0F;
    let masked = buf[1] & 0x80 != 0;
    let (len, mut offset) = match buf[1] & 0x7F {
        126 if buf.len() >= 4 => (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4),
        127 if buf.len() >= 10 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(bytes), 10)
        }
        126 | 127 => return Ok(None),
        len => (u64::from(len), 2),
    };
    if len > MAX_FRAME {
        bail!("WebSocket frame too large ({len} bytes)");
    }

    let mask = if masked {
        if buf.len() < offset + 4 {
            return Ok(None);
        }
        let mask = [buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]];
        offset += 4;
        Some(mask)
    } else {
        None
    };

    let len = usize::try_from(len)?;
    if buf.len() < offset + len {
        return Ok(None);
    }

    let mut payload: Vec<u8> = buf.drain(..offset + len).skip(offset).collect();
    if let Some(mask) = mask {
        apply_mask(&mut payload, mask);
    }

    Ok(Some(Frame { opcode, payload }))
}

/// A final, masked client frame (RFC 6455 5.2, 5.3).
fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);

    match payload.len() {
        len @ 0..=125 => frame.push(0x80 | len as u8),
        len @ 126..=0xFFFF => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }

    // A client must mask; a fresh key per frame is not needed for the relay, but is cheap.
    let mask = random_bytes::<4>().unwrap_or([0x5A, 0xC3, 0x3C, 0xA5]);
    frame.extend_from_slice(&mask);
    let start = frame.len();
    frame.extend_from_slice(payload);
    apply_mask(&mut frame[start..], mask);
    frame
}

fn apply_mask(data: &mut [u8], mask: [u8; 4]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("no system random source"))?;
    Ok(bytes)
}

fn encode_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 0x3F]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        bail!("odd number of hex digits");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).context("not a hex digit"))
        .collect()
}
