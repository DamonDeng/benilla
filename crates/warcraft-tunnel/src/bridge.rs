//! warcraft-tunnel bridge — a WebSocket↔TCP relay fronting the vmangos realmd/mangosd ports.
//!
//! It terminates plain `ws` (TLS is handled by the ALB in front of it) at `…/realm` and `…/world`,
//! checks a per-member bearer key on the handshake *before* it connects to any backend, then relays
//! raw bytes both ways. No valid key → no upgrade → the game ports are never reached. The game's own
//! SRP6 account login is the second factor, inside the tunnel.
//!
//! Config via env:
//!   KEYS_FILE      path to the TOML key register (default `keys.toml`)
//!   TUNNEL_LISTEN  bind address (default `127.0.0.1:9090`; the container image sets `0.0.0.0:9090`,
//!                  reached only by the load balancer on the private network)
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

/// The key register, hot-reloaded from `keys.toml` when its mtime changes (same pattern as the
/// cmangos-mcp adapter's `LiveTokens::reload_if_changed`). Adding or revoking a member is now just an
/// edit to the file — the next handshake picks it up; live relays are untouched; no bridge restart.
struct LiveKeys {
    path: String,
    mtime: Option<SystemTime>,
    members: Vec<KeyEntry>,
}

impl LiveKeys {
    fn file_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Reload only if the file's mtime changed since the last load. One stat per handshake — cheap
    /// (handshakes are ~2 per client session). SAFETY RULE: a missing/malformed/EMPTY file on reload
    /// keeps the OLD keys and logs the problem — a typo mid-edit must never lock every member out.
    /// (Revoking the last member isn't a real operation; you'd stop the bridge instead.)
    fn reload_if_changed(&mut self) {
        let cur = self.file_mtime();
        if cur == self.mtime {
            return;
        }
        match std::fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|t| toml::from_str::<KeysFile>(&t).map_err(|e| e.to_string()))
        {
            Ok(kf) if !kf.members.is_empty() => {
                if kf.members.len() != self.members.len() {
                    log(&format!(
                        "keys reloaded: {} -> {} members",
                        self.members.len(),
                        kf.members.len()
                    ));
                } else {
                    log(&format!("keys reloaded: {} members", kf.members.len()));
                }
                self.members = kf.members;
            }
            Ok(_) => log(&format!(
                "keys reload REFUSED: {} has no members; keeping the previous {} keys",
                self.path,
                self.members.len()
            )),
            Err(e) => log(&format!(
                "keys reload failed ({}): {e}; keeping the previous {} keys",
                self.path,
                self.members.len()
            )),
        }
        // Mark this mtime as seen either way, so a broken file doesn't re-log every handshake;
        // the next EDIT (new mtime) triggers another attempt.
        self.mtime = cur;
    }

    /// The owner whose key equals `presented`, checked in constant time against every entry (no
    /// early exit, so a timing side-channel can't reveal which prefix matched).
    fn owner_for(&self, presented: &str) -> Option<String> {
        let mut found: Option<String> = None;
        for m in &self.members {
            if ct_eq(presented.as_bytes(), m.key.as_bytes()) {
                found = Some(m.owner.clone());
            }
        }
        found
    }
}

struct Config {
    keys: Mutex<LiveKeys>,
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
            bail!("no keys configured in {keys_path}"); // startup still hard-fails: never start keyless
        }
        let mut keys = LiveKeys {
            path: keys_path,
            mtime: None,
            members: kf.members,
        };
        keys.mtime = keys.file_mtime();
        Ok(Config {
            keys: Mutex::new(keys),
            realm: std::env::var("REALMD_ADDR").unwrap_or_else(|_| "127.0.0.1:3724".into()),
            world: std::env::var("WORLD_ADDR").unwrap_or_else(|_| "127.0.0.1:8085".into()),
        })
    }

    /// Auth check for one handshake: refresh the register if the file changed, then match the key.
    fn owner_for(&self, presented: &str) -> Option<String> {
        let mut keys = self.keys.lock().unwrap();
        keys.reload_if_changed();
        keys.owner_for(presented)
    }

    fn key_count(&self) -> usize {
        self.keys.lock().unwrap().members.len()
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false; // keys are fixed-length, so a length check leaks nothing useful
    }
    let diff = a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y));
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
    let listen = std::env::var("TUNNEL_LISTEN").unwrap_or_else(|_| "127.0.0.1:9090".into());
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    log(&format!(
        "bridge listening on {listen} (keys: {}, hot-reload on, realm: {}, world: {})",
        cfg.key_count(),
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

// The handshake callback type (Result<Response, ErrorResponse>) is tungstenite's, so its large Err
// variant is not ours to shrink.
#[allow(clippy::result_large_err)]
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
                *d2.lock().unwrap() = Some((b, o));
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
    log(&format!(
        "ACCEPT owner={owner} peer={peer} -> {which} ({addr})"
    ));

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

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_file(name: &str, text: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("warcraft-tunnel-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("keys.toml");
        std::fs::write(&p, text).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn entry(owner: &str, key: &str) -> KeyEntry {
        KeyEntry {
            owner: owner.into(),
            key: key.into(),
        }
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn a_key_names_its_owner() {
        let k = LiveKeys {
            path: String::new(),
            mtime: None,
            members: vec![entry("a", "k1"), entry("b", "k2")],
        };
        assert_eq!(k.owner_for("k2").as_deref(), Some("b"));
        assert_eq!(k.owner_for("k3"), None);
        assert_eq!(k.owner_for(""), None);
    }

    #[test]
    fn reload_takes_a_good_file_and_keeps_the_old_keys_on_a_bad_one() {
        let path = keys_file(
            "reload",
            "[[members]]\nowner = \"a\"\nkey = \"k1\"\n[[members]]\nowner = \"b\"\nkey = \"k2\"\n",
        );
        let mut k = LiveKeys {
            path: path.clone(),
            mtime: None,
            members: vec![entry("old", "k0")],
        };
        k.reload_if_changed();
        assert_eq!(
            (k.members.len(), k.owner_for("k2").as_deref()),
            (2, Some("b"))
        );
        for bad in ["members = []\n", "not toml [[", ""] {
            std::fs::write(&path, bad).unwrap();
            k.mtime = None; // as if the edit changed the mtime
            k.reload_if_changed();
            assert_eq!(k.members.len(), 2, "kept the previous keys after {bad:?}");
        }
        std::fs::remove_file(&path).unwrap();
        k.mtime = Some(SystemTime::UNIX_EPOCH);
        k.reload_if_changed();
        assert_eq!(k.members.len(), 2, "a missing file keeps the previous keys");
    }
}
