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
| `TUNNEL_LISTEN` | `0.0.0.0:9090` | the ALB target port |
| `REALMD_ADDR` | `127.0.0.1:3724` | `realmd:3724` under compose |
| `WORLD_ADDR` | `127.0.0.1:8085` | `mangosd:8085` under compose |

## Build / run

```sh
cargo build --release                      # local
docker build -t warcraft-tunnel .          # image (deployed beside vmangos in compose)
```
