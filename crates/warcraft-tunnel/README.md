# warcraft-tunnel

A small WebSocket↔TCP **bridge** that fronts the vmangos `realmd` (auth) and `mangosd` (world)
ports so the WoW protocol can travel inside `wss`-on-443. It exists because the 1.12 client speaks
raw TCP, not HTTP/TLS — so we wrap it.

## How it fits

```
benilla (WsTunnelTransport)
   │  wss://<domain>/<special_url>/{realm,world}   Authorization: Bearer <key>
   ▼
 ALB :443  (ACM TLS; Cognito gates "/", /<special_url> is the exception)
   │  ws  (TLS terminated at the ALB; private network from here)
   ▼
 bridge :9090  ── validates the per-member key, then relays raw bytes ──►  realmd:3724 / mangosd:8085
```

- **No valid key → no upgrade → the game ports are never touched.** The game's SRP6 account login is
  the second factor, inside the tunnel.
- The bridge is an **opaque byte relay** — it never parses the WoW protocol.
- TLS is handled by the ALB; the bridge itself speaks plain `ws` on the private network.

## Auth: per-member keys

Keys live in `keys.toml` (see `../env_info/keys.toml`), one entry per person/bot:

```toml
[[members]]
owner = "<username>"
key   = "<64 hex chars>"
```

The key is presented as `Authorization: Bearer <key>` on the handshake. Lookup is constant-time.
The access log names the `owner` of every accepted connection.

**Hot reload:** the bridge re-reads `keys.toml` whenever its mtime changes (checked with one `stat`
per handshake). Add a member by appending a row; revoke one by deleting their row — the next
handshake honors the change, live relays are untouched, **no restart needed**. Safety rule: a
missing, malformed, or *empty* file on reload keeps the previous keys and logs the problem (a typo
mid-edit can't lock everyone out). Startup still refuses to run without valid keys.

> **Docker gotcha:** if `keys.toml` is bind-mounted as a *single file*, editors that replace-by-rename
> (`vim`, `sed -i`) swap the inode and the container keeps seeing the old file — the reload silently
> never fires. Either edit in place (`cat > keys.toml` / append with `>>`), or mount the parent
> *directory* and point `KEYS_FILE` into it. A restart still picks up any change, as before.

## Config (env)

| Var | Default | Notes |
|---|---|---|
| `KEYS_FILE` | `keys.toml` | mounted from `env_info/keys.toml` |
| `TUNNEL_LISTEN` | `127.0.0.1:9090` | the ALB target port; the image sets `0.0.0.0:9090` (only the ALB reaches it, on the private network) |
| `REALMD_ADDR` | `127.0.0.1:3724` | `realmd:3724` under compose |
| `WORLD_ADDR` | `127.0.0.1:8085` | `mangosd:8085` under compose |

## Build / run

This crate lives in the benilla workspace (it was the `warcraft-tunnel` repo; history kept by
`git subtree`). Build from the workspace root:

```sh
cargo build --release -p warcraft-tunnel                                # -> target/release/bridge
cargo test -p warcraft-tunnel                                           # key check and hot reload
docker build -f crates/warcraft-tunnel/Dockerfile -t warcraft-tunnel .  # image (beside vmangos in compose)
```

The image name, its environment variables and the `keys.toml` mount are unchanged, so a compose
file that ran the old image runs this one.

## Client side: one file turns the tunnel on

The GUI client (`benilla-app`) and the headless bots (`warcraft-army`) read the same file. With it
they connect through the bridge; without it they dial `realmd` / `mangosd` directly, as before.

```toml
# ~/.config/benilla/tunnel.toml  (chmod 600; never commit it)
endpoint  = "wss://<domain>"     # or ws://127.0.0.1:9090 for a local bridge
base_path = "/<special_url>"     # may be empty
key       = "<your member key>"
```

Where the client looks, first hit wins (`benilla-protocol/src/tunnel_config.rs`):

1. `BENILLA_TUNNEL_CONFIG`: the path of the file;
2. `ARMY_TUNNEL_CONFIG`: the army's older name for it;
3. `BENILLA_TUNNEL_URL` + `BENILLA_TUNNEL_KEY` (+ `BENILLA_TUNNEL_PATH`): the older environment-only form;
4. `$XDG_CONFIG_HOME/benilla/tunnel.toml`, else `~/.config/benilla/tunnel.toml` (`%APPDATA%\benilla\tunnel.toml` on Windows).

At the first connect the client prints one line, `tunnel: connecting through <endpoint> (settings
from <file>)`; never the key or the path. A file that is named but broken (no key, not a
`ws://`/`wss://` endpoint) gets a line on stderr and the connect stays direct. A file readable by
others gets a warning to `chmod 600` it.
