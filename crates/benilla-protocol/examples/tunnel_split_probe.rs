//! Headless check that the GUI's read/write SPLIT works over the `wss` tunnel: logon + world
//! connect over the tunnel, `into_split()`, then a CONCURRENT writer (sends a ping) + reader
//! (another thread, blocks for the pong) — the exact pattern that would deadlock a naive single
//! mutex — then `rejoin()`. Splitting at character-select needs no character on the account.
//!
//! Run:
//!   BENILLA_TUNNEL_URL=wss://wow.agentnumber47.com \
//!   BENILLA_TUNNEL_PATH=<special_url> \
//!   BENILLA_TUNNEL_KEY=<bearer key> \
//!   PROBE_USER=<username> PROBE_PASS=<password> \
//!   cargo run -p benilla-protocol --example tunnel_split_probe

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use benilla_protocol::{auth, logon_tunneled, Poll, SessionEvent, TunnelConfig, WorldSession};

fn main() -> Result<()> {
    let cfg = TunnelConfig::from_env()
        .context("set BENILLA_TUNNEL_URL and BENILLA_TUNNEL_KEY (and BENILLA_TUNNEL_PATH)")?;
    let user = std::env::var("PROBE_USER").unwrap_or_else(|_| "<username>".into());
    let pass = std::env::var("PROBE_PASS").unwrap_or_else(|_| "<password>".into());

    eprintln!("tunnel {} (path {})", cfg.endpoint, cfg.base_path);
    let logon = logon_tunneled(&cfg, &user, &pass, auth::LOCALE_TAG_EN_US).context("logon")?;
    let session =
        WorldSession::connect_tunneled(&cfg, &user, logon.session_key).context("world connect")?;
    println!("connected over tunnel (at character select)");

    // Split, then drive the two halves from two threads at once.
    let (mut reader, mut writer) = session.into_split().context("into_split over tunnel")?;
    let (tx, rx) = mpsc::channel::<(benilla_protocol::WorldReader, bool)>();
    std::thread::spawn(move || {
        let mut got_pong = false;
        for _ in 0..500 {
            match reader.poll() {
                Ok(Poll::Events { events, .. }) => {
                    if events.iter().any(|e| matches!(e, SessionEvent::Pong { .. })) {
                        got_pong = true;
                        break;
                    }
                }
                Ok(Poll::Skipped { .. }) => {}
                Err(_) => break, // link closed
            }
        }
        let _ = tx.send((reader, got_pong));
    });

    std::thread::sleep(Duration::from_millis(300)); // let the reader thread settle
    writer.ping(1, 0).context("sending CMSG_PING on the split writer")?;
    println!("ping sent on the writer half; waiting for the pong on the reader half...");

    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok((reader, got_pong)) => {
            let _session = WorldSession::rejoin(reader, writer).context("rejoin over tunnel")?;
            if got_pong {
                println!("SPLIT_OK: concurrent reader+writer over the tunnel, ping round-tripped, split + rejoin succeeded");
            } else {
                println!("SPLIT_PARTIAL: split + rejoin worked, but no pong was observed");
            }
        }
        Err(_) => println!("SPLIT_TIMEOUT: no pong within 8s (reader still blocked) — investigate"),
    }
    Ok(())
}
