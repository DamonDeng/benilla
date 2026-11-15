//! Headless end-to-end check: authenticate and enter the world *over the wss tunnel*, then read
//! the character roster. Proves wss/443 -> ALB -> bridge -> realmd/mangosd -> SRP6 works.
//!
//! Run (key stays in the env, never on the command line in cleartext here):
//!   BENILLA_TUNNEL_URL=wss://wow.agentnumber47.com \
//!   BENILLA_TUNNEL_PATH=<special_url> \
//!   BENILLA_TUNNEL_KEY=<bearer key> \
//!   PROBE_USER=<username> PROBE_PASS=<password> \
//!   cargo run -p benilla-protocol --example tunnel_probe

use anyhow::{Context, Result};
use benilla_protocol::{auth, logon_tunneled, TunnelConfig, WorldSession};

fn main() -> Result<()> {
    let cfg = TunnelConfig::from_env()
        .context("set BENILLA_TUNNEL_URL and BENILLA_TUNNEL_KEY (and BENILLA_TUNNEL_PATH)")?;
    let user = std::env::var("PROBE_USER").unwrap_or_else(|_| "<username>".into());
    let pass = std::env::var("PROBE_PASS").unwrap_or_else(|_| "<password>".into());

    eprintln!("tunnel endpoint: {} (path {})", cfg.endpoint, cfg.base_path);
    let l = logon_tunneled(&cfg, &user, &pass, auth::LOCALE_TAG_EN_US).context("logon over tunnel")?;
    println!(
        "LOGON OK over tunnel: {} realm(s), session key {} bytes",
        l.realms.len(),
        l.session_key.len()
    );

    let mut ws =
        WorldSession::connect_tunneled(&cfg, &user, l.session_key).context("world connect over tunnel")?;
    let roster = ws.char_enum().context("char_enum over tunnel")?;
    let names: Vec<&str> = roster.iter().map(|c| c.name.as_str()).collect();
    println!("TUNNEL LOGIN OK: {} character(s) for {user}: {names:?}", roster.len());
    Ok(())
}
