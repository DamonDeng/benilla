//! Where the `wss` tunnel settings come from: one small TOML file. The file being there turns
//! the tunnel on; no file means the stock direct TCP connect. Shared by the GUI client
//! (`benilla-app`) and the headless bots (`warcraft-army`), so both read the same settings.
//!
//! The file (`tunnel.toml`, keep it out of any repo, mode 600):
//!
//! ```toml
//! endpoint  = "wss://example.com"   # or ws://127.0.0.1:9090 for a local bridge
//! base_path = "/<path>"             # the bridge's non-guessable path; may be empty
//! key       = "<key>"               # the per-member bearer key: a secret
//! ```
//!
//! Lookup order, first hit wins:
//! 1. `BENILLA_TUNNEL_CONFIG`: the path of the file;
//! 2. `ARMY_TUNNEL_CONFIG`: the army's older name for the same file;
//! 3. `BENILLA_TUNNEL_URL` + `BENILLA_TUNNEL_KEY` (+ `BENILLA_TUNNEL_PATH`): the older
//!    environment-only form;
//! 4. `<config dir>/benilla/tunnel.toml`, where the config dir is `$XDG_CONFIG_HOME`, else
//!    `$HOME/.config` (`%APPDATA%` on Windows).
//!
//! A file that is named but unreadable or incomplete is a mistake worth a line on stderr; the
//! connect then stays direct, as with no file at all.

use std::path::{Path, PathBuf};

use crate::transport::TunnelConfig;

/// Where a [`TunnelConfig`] came from, for the one line the client prints at connect time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelSource {
    /// A file: named by an environment variable, or the default location.
    File(PathBuf),
    /// The older `BENILLA_TUNNEL_URL` / `_KEY` / `_PATH` variables.
    Env,
}

impl TunnelConfig {
    /// The tunnel settings by the lookup order in the module docs, or `None` for direct TCP.
    pub fn load() -> Option<(Self, TunnelSource)> {
        load_with(&|k| std::env::var(k).ok(), default_file())
    }

    /// [`Self::load`], with one stderr line the first time it finds settings: the endpoint and
    /// where they came from (never the key or the path). What the client and the army call.
    pub fn configured() -> Option<Self> {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let found = Self::load();
        if let Some((cfg, src)) = &found {
            ONCE.call_once(|| {
                let from = match src {
                    TunnelSource::File(p) => p.display().to_string(),
                    TunnelSource::Env => "BENILLA_TUNNEL_URL/_KEY".to_string(),
                };
                eprintln!(
                    "tunnel: connecting through {} (settings from {from})",
                    cfg.endpoint
                );
            });
        }
        found.map(|(cfg, _)| cfg)
    }

    /// Parse a tunnel TOML: `endpoint`, `base_path` and `key`, each `name = "value"` on its own
    /// line; `#` comments and blank lines are skipped, unknown names are ignored. Hand-parsed so
    /// the protocol crate takes no TOML dependency for three strings.
    pub fn parse_toml(text: &str) -> Result<Self, String> {
        let (mut endpoint, mut base_path, mut key) = (String::new(), String::new(), String::new());
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v
                .split(" #")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string();
            match k.trim() {
                "endpoint" => endpoint = v,
                "base_path" => base_path = v,
                "key" => key = v,
                _ => {}
            }
        }
        if !(endpoint.starts_with("wss://") || endpoint.starts_with("ws://")) {
            return Err("endpoint must be a ws:// or wss:// URL".into());
        }
        if key.is_empty() {
            return Err("key is missing".into());
        }
        if !base_path.is_empty() && !base_path.starts_with('/') {
            return Err("base_path must start with /".into());
        }
        Ok(Self {
            endpoint,
            base_path,
            key,
        })
    }

    /// Read and parse a tunnel TOML file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        warn_if_readable_by_others(path);
        Self::parse_toml(&text)
    }
}

/// `<config dir>/benilla/tunnel.toml`, or `None` when no config dir is known.
fn default_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("benilla").join("tunnel.toml"))
}

fn load_with(
    var: &dyn Fn(&str) -> Option<String>,
    default: Option<PathBuf>,
) -> Option<(TunnelConfig, TunnelSource)> {
    for name in ["BENILLA_TUNNEL_CONFIG", "ARMY_TUNNEL_CONFIG"] {
        if let Some(p) = var(name).filter(|v| !v.is_empty()) {
            let path = PathBuf::from(p);
            return match TunnelConfig::from_file(&path) {
                Ok(cfg) => Some((cfg, TunnelSource::File(path))),
                Err(e) => {
                    eprintln!("tunnel: {name} = {}: {e}; using direct TCP", path.display());
                    None
                }
            };
        }
    }
    if let (Some(endpoint), Some(key)) = (var("BENILLA_TUNNEL_URL"), var("BENILLA_TUNNEL_KEY")) {
        let base_path = var("BENILLA_TUNNEL_PATH").unwrap_or_default();
        return Some((
            TunnelConfig {
                endpoint,
                base_path,
                key,
            },
            TunnelSource::Env,
        ));
    }
    let path = default?;
    if !path.is_file() {
        return None;
    }
    match TunnelConfig::from_file(&path) {
        Ok(cfg) => Some((cfg, TunnelSource::File(path))),
        Err(e) => {
            eprintln!("tunnel: {}: {e}; using direct TCP", path.display());
            None
        }
    }
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(path) {
        if m.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "tunnel: {} holds a secret key but is readable by others; chmod 600 it",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn file(text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "benilla-tunnel-{}-{}",
            std::process::id(),
            text.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tunnel.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    const GOOD: &str = "# a comment\nendpoint = \"wss://example.com\"\nbase_path = \"/t-ab12\"\nkey = \"k1\"  # inline\nother = \"x\"\n";

    #[test]
    fn parses_the_three_names_and_skips_the_rest() {
        let c = TunnelConfig::parse_toml(GOOD).unwrap();
        assert_eq!(
            (c.endpoint.as_str(), c.base_path.as_str(), c.key.as_str()),
            ("wss://example.com", "/t-ab12", "k1")
        );
        assert_eq!(
            c.url(crate::transport::Leg::Realm),
            "wss://example.com/t-ab12/realm"
        );
    }

    #[test]
    fn rejects_a_file_that_cannot_work() {
        assert!(TunnelConfig::parse_toml("endpoint = \"https://x\"\nkey = \"k\"").is_err());
        assert!(TunnelConfig::parse_toml("endpoint = \"wss://x\"").is_err());
        assert!(
            TunnelConfig::parse_toml("endpoint = \"wss://x\"\nkey = \"k\"\nbase_path = \"t\"")
                .is_err()
        );
        let c =
            TunnelConfig::parse_toml("endpoint = \"ws://127.0.0.1:9090\"\nkey = \"k\"").unwrap();
        assert_eq!(c.base_path, "");
    }

    #[test]
    fn no_settings_means_direct_tcp() {
        assert!(load_with(&vars(&[]), None).is_none());
        assert!(load_with(
            &vars(&[]),
            Some(PathBuf::from("/nonexistent/benilla/tunnel.toml"))
        )
        .is_none());
    }

    #[test]
    fn the_named_file_wins_then_the_army_name_then_the_env_then_the_default() {
        let p = file(GOOD);
        let ps = p.to_str().unwrap();
        let env = [
            ("BENILLA_TUNNEL_URL", "wss://env"),
            ("BENILLA_TUNNEL_KEY", "ek"),
        ];
        let (c, s) = load_with(
            &vars(&[("BENILLA_TUNNEL_CONFIG", ps), env[0], env[1]]),
            None,
        )
        .unwrap();
        assert_eq!((c.key.as_str(), s), ("k1", TunnelSource::File(p.clone())));
        let (c, _) = load_with(&vars(&[("ARMY_TUNNEL_CONFIG", ps)]), None).unwrap();
        assert_eq!(c.key, "k1");
        let (c, s) = load_with(&vars(&env), Some(p.clone())).unwrap();
        assert_eq!((c.endpoint.as_str(), s), ("wss://env", TunnelSource::Env));
        let (c, s) = load_with(&vars(&[]), Some(p.clone())).unwrap();
        assert_eq!((c.key.as_str(), s), ("k1", TunnelSource::File(p)));
    }

    #[test]
    fn a_named_but_broken_file_stays_direct() {
        let p = file("endpoint = \"wss://x\"\n");
        assert!(load_with(
            &vars(&[("BENILLA_TUNNEL_CONFIG", p.to_str().unwrap())]),
            None
        )
        .is_none());
        assert!(load_with(
            &vars(&[("BENILLA_TUNNEL_CONFIG", "/nonexistent/t.toml")]),
            None
        )
        .is_none());
    }
}
