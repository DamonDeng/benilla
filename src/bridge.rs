//! warcraft-tunnel bridge — a WebSocket↔TCP relay fronting the vmangos realmd/mangosd ports.
//!
//! It terminates plain `ws` (TLS is handled by the ALB in front of it) at `…/realm` and `…/world`,
//! checks a per-member bearer key on the handshake *before* it connects to any backend, then relays
//! raw bytes both ways. No valid key → no upgrade → the game ports are never reached. The game's own
//! SRP6 account login is the second factor, inside the tunnel.
//!
//! Config via env:
//!   KEYS_FILE      path to the TOML key register (default `keys.toml`)
//!   TUNNEL_LISTEN  bind address (default `0.0.0.0:9090`)
//!   REALMD_ADDR    realmd backend  (default `127.0.0.1:3724`; in compose: `realmd:3724`)
//!   WORLD_ADDR     mangosd backend (default `127.0.0.1:8085`; in compose: `mangosd:8085`)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

#[derive(Deserialize)]
struct KeyEntry {
    owner: String,
    key: String,
}

#[derive(Deserialize)]
struct KeysFile {
    members: Vec<KeyEntry>,
}

struct Config {
    members: Vec<KeyEntry>,
    realm: String,
    world: String,
}

impl Config {
    fn load() -> Result<Self> {
        let keys_path = std::env::var("KEYS_FILE").unwrap_or_else(|_| "keys.toml".into());
        let text = std::fs::read_to_string(&keys_path)
            .with_context(|| format!("reading keys file {keys_path}"))?;
        let kf: KeysFile = toml::from_str(&text).context("parsing keys file")?;
        if kf.members.is_empty() {
            bail!("no keys configured in {keys_path}");
        }
        Ok(Config {
            members: kf.members,
            realm: std::env::var("REALMD_ADDR").unwrap_or_else(|_| "127.0.0.1:3724".into()),
            world: std::env::var("WORLD_ADDR").unwrap_or_else(|_| "127.0.0.1:8085".into()),
        })
    }

    /// The owner whose key equals `presented`, checked in constant time against every entry (no
    /// early exit, so a timing side-channel can't reveal which prefix matched).
    fn owner_for(&self, presented: &str) -> Option<&str> {
        let mut found: Option<&str> = None;
        for m in &self.members {
            if ct_eq(presented.as_bytes(), m.key.as_bytes()) {
                found = Some(&m.owner);
            }
        }
        found
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false; // keys are fixed-length, so a length check leaks nothing useful
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn log(msg: &str) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("[{t}] {msg}");
}

enum Backend {
    Realm,
    World,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(Config::load()?);
    let listen = std::env::var("TUNNEL_LISTEN").unwrap_or_else(|_| "0.0.0.0:9090".into());
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    log(&format!(
        "bridge listening on {listen} (keys: {}, realm: {}, world: {})",
        cfg.members.len(),
        cfg.realm,
        cfg.world
    ));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                log(&format!("accept error: {e}"));
                continue;
            }
        };
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, peer, cfg).await {
                log(&format!("conn {peer} ended: {e}"));
            }
        });
    }
}

async fn handle(stream: TcpStream, peer: SocketAddr, cfg: Arc<Config>) -> Result<()> {
    // Fast path for the ALB target-group health check: a plain `GET /healthz` (not a WS upgrade).
    // We peek WITHOUT consuming, so a real WS request still reaches the handshake fully intact.
    let mut probe = [0u8; 32];
    if let Ok(n) = stream.peek(&mut probe).await {
        if probe[..n].starts_with(b"GET /healthz") {
            let mut s = stream;
            let mut sink = [0u8; 2048];
            let _ = s.read(&mut sink).await; // drain the request (best effort)
            let _ = s
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await;
            let _ = s.shutdown().await;
            return Ok(());
        }
    }

    // Validate the key + route during the handshake; stash the decision for use after the upgrade.
    let decided: Arc<Mutex<Option<(Backend, String)>>> = Arc::new(Mutex::new(None));
    let d2 = decided.clone();
    let cfg2 = cfg.clone();

    let ws = accept_hdr_async(stream, move |req: &Request, resp: Response| {
        let key = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        let owner = cfg2.owner_for(key);
        let path = req.uri().path();
        let backend = if path.ends_with("/realm") {
            Some(Backend::Realm)
        } else if path.ends_with("/world") {
            Some(Backend::World)
        } else {
            None
        };
        match (owner, backend) {
            (Some(o), Some(b)) => {
                *d2.lock().unwrap() = Some((b, o.to_string()));
                Ok(resp)
            }
            _ => {
                let body = Some("unauthorized".to_string());
                let deny = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(body)
                    .unwrap();
                Err::<Response, ErrorResponse>(deny)
            }
        }
    })
    .await
    .context("ws handshake rejected or failed")?;

    let (backend, owner) = decided.lock().unwrap().take().context("no route decided")?;
    let (addr, which) = match backend {
        Backend::Realm => (cfg.realm.clone(), "realm"),
        Backend::World => (cfg.world.clone(), "world"),
    };
    log(&format!("ACCEPT owner={owner} peer={peer} -> {which} ({addr})"));

    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    relay(ws, tcp).await;
    log(&format!("CLOSE owner={owner} peer={peer} ({which})"));
    Ok(())
}

/// Pump bytes both directions until either side closes. Opaque: WS binary frames carry arbitrary
/// chunks of the TCP stream; we never parse the WoW protocol.
async fn relay(mut ws: tokio_tungstenite::WebSocketStream<TcpStream>, tcp: TcpStream) {
    let (mut trd, mut twr) = tcp.into_split();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if twr.write_all(&data).await.is_err() { break; }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if ws.send(Message::Pong(p)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ignore text/pong
                    Some(Err(_)) => break,
                }
            }
            n = trd.read(&mut buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        if ws.send(Message::Binary(buf[..n].to_vec())).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    let _ = ws.close(None).await;
}
