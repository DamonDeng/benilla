//! An optional WebSocket tunnel for the auth and world connections.
//!
//! The 1.12 client speaks raw TCP to `realmd` (3724) and `mangosd` (8085). To reach a server that
//! is only exposed through our `wss`-on-443 bridge (which wraps those ports so the traffic survives
//! corporate firewalls), a connection can instead be a [`WsTunnelTransport`]: a WebSocket whose
//! binary frames carry the exact same WoW byte stream. The bridge is an opaque relay, so nothing
//! about the protocol changes — only the pipe.
//!
//! [`Transport`] is the abstraction: a connection is **either** a direct [`TcpStream`] (the default,
//! unchanged behaviour) **or** a [`WsTunnelTransport`]. Both implement [`Read`] + [`Write`], so they
//! drop into `recv_packet`/`send_packet`, which are already generic over `Read`/`Write`.
//!
//! Wire contract (must match the bridge):
//! - open `wss://<host>/<base>/realm` for the logon/realmd leg, `.../world` for the world leg;
//! - send `Authorization: Bearer <key>` on the handshake (a bad/missing key → HTTP 401);
//! - once open, each WebSocket **binary** frame is an opaque chunk of the TCP byte stream.
//!
//! The tunnel is **not** wired into [`crate::logon`] or [`crate::WorldSession::connect`] yet; see the
//! module's accompanying report for the exact (small) plumbing that selects it. Default stays TCP.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::header::AUTHORIZATION;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Which leg of the connection a tunnel carries. The bridge routes on the path suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// The SRP6 logon / realm-list leg → `realmd:3724`.
    Realm,
    /// The world leg → `mangosd:8085`.
    World,
}

impl Leg {
    fn as_str(self) -> &'static str {
        match self {
            Leg::Realm => "realm",
            Leg::World => "world",
        }
    }
}

/// How to reach the `wss` bridge. The per-identity `key` is a bearer secret and must not be
/// hard-coded — pass it from the app's config (e.g. the army's `army-secrets.toml`) or the
/// environment via [`TunnelConfig::from_env`].
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// Scheme + host[:port], e.g. `wss://wow.agentnumber47.com` or `ws://127.0.0.1:9090` (plain
    /// `ws` for local bridge testing through an SSM port-forward, before the TLS edge exists).
    pub endpoint: String,
    /// The bridge's non-guessable base path, e.g. `/t-ab12cd`. The leg (`/realm`, `/world`) is
    /// appended. May be empty.
    pub base_path: String,
    /// The per-member bearer key the bridge checks on the handshake.
    pub key: String,
}

impl TunnelConfig {
    /// Read a config from the environment, or `None` if the tunnel isn't configured:
    /// `BENILLA_TUNNEL_URL` (required), `BENILLA_TUNNEL_KEY` (required), `BENILLA_TUNNEL_PATH`
    /// (optional base path). Absence of the first two means "use direct TCP".
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("BENILLA_TUNNEL_URL").ok()?;
        let key = std::env::var("BENILLA_TUNNEL_KEY").ok()?;
        let base_path = std::env::var("BENILLA_TUNNEL_PATH").unwrap_or_default();
        Some(Self {
            endpoint,
            base_path,
            key,
        })
    }

    /// The full `ws(s)://…/<leg>` URL for a leg.
    pub fn url(&self, leg: Leg) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let path = self.base_path.trim_end_matches('/');
        format!("{base}{path}/{}", leg.as_str())
    }
}

/// A WoW byte stream carried over a WebSocket to the bridge. Implements [`Read`]/[`Write`] by
/// mapping to/from binary frames, buffering partial reads, and answering Pings with Pongs.
pub struct WsTunnelTransport {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    /// A dup of the underlying socket (shares the fd), kept so [`Self::set_read_timeout`] can set
    /// `SO_RCVTIMEO` on the socket that `ws` reads from — the same mechanism the direct path uses.
    sock: TcpStream,
    /// Leftover bytes from a frame larger than the caller's last `read` buffer.
    rbuf: Vec<u8>,
    rpos: usize,
}

impl WsTunnelTransport {
    /// Open a tunnel for `leg`, presenting the config's bearer key on the handshake.
    pub fn connect(cfg: &TunnelConfig, leg: Leg) -> Result<Self> {
        let url = cfg.url(leg);
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("building tunnel request for {url}"))?;
        let bearer = format!("Bearer {}", cfg.key);
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&bearer).context("encoding the tunnel key as a header")?,
        );

        let uri = request.uri().clone();
        let scheme = uri.scheme_str().unwrap_or("wss");
        let tls = scheme == "wss";
        let host = uri
            .host()
            .ok_or_else(|| anyhow!("tunnel URL has no host: {url}"))?
            .to_string();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

        // Own the TcpStream so we keep a dup for timeout control; share the fd with the ws socket.
        let tcp = TcpStream::connect((host.as_str(), port))
            .with_context(|| format!("connecting to the tunnel at {host}:{port}"))?;
        tcp.set_nodelay(true).ok();
        let sock = tcp.try_clone().context("duplicating the tunnel socket")?;

        // `client`/`client_tls` have different error types, so map each by Display (a bad/missing
        // key shows as the bridge's HTTP 401 in the message).
        let (ws, _response) = if tls {
            tungstenite::client_tls(request, tcp)
                .map_err(|e| anyhow!("tunnel TLS handshake failed: {e}"))?
        } else {
            tungstenite::client(request, MaybeTlsStream::Plain(tcp))
                .map_err(|e| anyhow!("tunnel handshake failed: {e}"))?
        };

        Ok(Self {
            ws,
            sock,
            rbuf: Vec::new(),
            rpos: 0,
        })
    }

    /// Set `SO_RCVTIMEO` on the underlying socket. `None` makes reads block. Because the ws socket
    /// and `sock` are dups of the same fd, this governs the timeouts tungstenite's reads observe —
    /// so a quiet stream surfaces as an `io::Error` of kind `WouldBlock`/`TimedOut`, exactly as a
    /// direct `TcpStream` read would. Partial frames are preserved by tungstenite across a timeout,
    /// so a later read resumes cleanly.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }

    /// Put the underlying socket in (non-)blocking mode, for a non-blocking liveness peek.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.sock.set_nonblocking(nonblocking)
    }

    /// Peek bytes pending on the underlying socket without consuming them. These are **raw**
    /// transport bytes (TLS/WebSocket framing), not the decoded WoW stream, so this is a liveness
    /// signal only: `Ok(0)` = the link closed, `Ok(n)` = something is pending, a `WouldBlock`
    /// error = quiet. A caller that then reads expects whole ws frames to follow promptly.
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.sock.peek(buf)
    }
}

impl Read for WsTunnelTransport {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            // Drain any leftover frame bytes first.
            if self.rpos < self.rbuf.len() {
                let n = out.len().min(self.rbuf.len() - self.rpos);
                out[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
                self.rpos += n;
                if self.rpos == self.rbuf.len() {
                    self.rbuf.clear();
                    self.rpos = 0;
                }
                return Ok(n);
            }
            // Otherwise pull the next frame.
            match self.ws.read() {
                Ok(Message::Binary(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    self.rbuf = data;
                    self.rpos = 0;
                }
                Ok(Message::Ping(payload)) => {
                    // Reply so the bridge/ALB don't reap us; ignore a write failure (the next
                    // read/write will surface the real error).
                    let _ = self.ws.send(Message::Pong(payload));
                }
                Ok(Message::Close(_)) => return Ok(0), // EOF, like a half-closed TCP read
                Ok(_) => continue,                     // Text/Pong/raw frames: not our protocol
                Err(tungstenite::Error::Io(e)) => return Err(e), // includes WouldBlock/TimedOut
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(0)
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
            }
        }
    }
}

impl Write for WsTunnelTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // `send` writes the frame and flushes it.
        self.ws
            .send(Message::Binary(buf.to_vec()))
            .map_err(to_io)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ws.flush().map_err(to_io)
    }
}

/// A connection the protocol reads/writes: the default direct socket, or the `wss` tunnel.
///
/// `Transport` implements [`Read`] + [`Write`], so it slots into the existing generic packet IO.
/// `set_read_timeout`/`set_nodelay` mirror the `TcpStream` calls the connect paths make today.
///
/// NOTE on splitting: [`crate::WorldSession::into_split`] relies on `TcpStream::try_clone` to make
/// independent read/write halves. A single WebSocket cannot be cloned that way, so split is only
/// available on the `Tcp` variant (see [`Transport::try_clone_tcp`]); a tunnelled world session
/// that needs the split is a design decision flagged in the report (e.g. share the ws behind a
/// mutex, or run the two legs as two tunnels).
pub enum Transport {
    /// The default: a direct TCP socket, byte-for-byte the prior behaviour.
    Tcp(TcpStream),
    /// The `wss` bridge tunnel.
    Ws(WsTunnelTransport),
}

impl Transport {
    /// Open a tunnelled transport for `leg`.
    pub fn connect_tunnel(cfg: &TunnelConfig, leg: Leg) -> Result<Self> {
        Ok(Transport::Ws(WsTunnelTransport::connect(cfg, leg)?))
    }

    /// Set the read timeout on whichever backing socket this is.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.set_read_timeout(timeout),
            Transport::Ws(w) => w.set_read_timeout(timeout),
        }
    }

    /// `TCP_NODELAY`. A no-op for the tunnel (the bridge's own sockets set it).
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.set_nodelay(nodelay),
            Transport::Ws(_) => Ok(()),
        }
    }

    /// Clone the socket for a read/write split — only possible on the direct `Tcp` variant.
    pub fn try_clone_tcp(&self) -> io::Result<TcpStream> {
        match self {
            Transport::Tcp(s) => s.try_clone(),
            Transport::Ws(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a WebSocket tunnel cannot be split with try_clone",
            )),
        }
    }

    /// Non-blocking mode on the backing socket, for a liveness peek.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.set_nonblocking(nonblocking),
            Transport::Ws(w) => w.set_nonblocking(nonblocking),
        }
    }

    /// Peek pending bytes without consuming them (a liveness signal only on the tunnel — see
    /// [`WsTunnelTransport::peek`]).
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.peek(buf),
            Transport::Ws(w) => w.peek(buf),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.read(buf),
            Transport::Ws(w) => w.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.write(buf),
            Transport::Ws(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.flush(),
            Transport::Ws(w) => w.flush(),
        }
    }
}

fn to_io(e: tungstenite::Error) -> io::Error {
    match e {
        tungstenite::Error::Io(io) => io,
        other => io::Error::new(io::ErrorKind::Other, other),
    }
}
