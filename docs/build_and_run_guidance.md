# Building and running benilla

How to build the client, start it against a server directly or over the `wss` tunnel, and what
to check when the login or the connection fails. Each command is given for bash (macOS, Linux)
and for PowerShell (Windows). The README's "Running it" section is the short version;
[`CONTRIBUTING.md`](CONTRIBUTING.md) has the test and probe switches.

## 1. What you need

- **A 1.12.1 client (build 5875)**, any locale. benilla reads its `Data` folder and never writes
  into it.
- **A 1.12.1 server**: vmangos, cMaNGOS or another 1.12.1 core. benilla does not answer Warden,
  so a server that enforces Warden disconnects the session after its timeout.
- **Stable Rust and a C compiler.** `rust-toolchain.toml` pins the toolchain and adds clippy and
  rustfmt; `rustup toolchain install` in the repo installs that toolchain.
  - macOS: the Xcode command line tools.
  - Linux: the ALSA and udev development packages and `pkg-config` (for example
    `alsa-lib-devel systemd-devel` on Fedora and Amazon Linux, `libasound2-dev libudev-dev` on
    Debian and Ubuntu).
  - Windows: the MSVC build tools that the Rust installer sets up.

## 2. Building

The `play` profile is `--release` optimization plus incremental compilation, and is what
`play.sh` and the `cargo play` alias (`.cargo/config.toml`) use. The first build compiles Bevy
and takes a while.

```sh
cargo build --profile play -p benilla             # the client:   target/play/benilla
cargo build --profile play -p warcraft-explorer   # explore mode: target/play/warcraft-explorer
```

```powershell
cargo build --profile play -p benilla             # target\play\benilla.exe
cargo build --profile play -p warcraft-explorer   # target\play\warcraft-explorer.exe
```

These are dev builds (the `dev` feature is on by default): they find a `WoW` link at the repo
root and keep their state in `benilla-config/` at the repo root (§3). A player build,
`cargo build --profile play -p benilla --no-default-features`, looks only beside its binary.

## 3. Running (macOS, Linux)

Tell the client where the install is, either with `WOW_DATA` or with a `WoW` link at the repo
root:

```sh
ln -s /path/to/WoW WoW                                  # once; or set WOW_DATA each run
WOW_DATA=/path/to/WoW/Data cargo play                   # build if needed, then run
WOW_DATA=/path/to/WoW/Data ./target/play/benilla        # run the built binary
```

`play.sh` wraps the two common runs:

```sh
./play.sh                                  # connect: builds target/play/benilla if missing
WOW_USER=<account> WOW_PASS=<password> ./play.sh
./play.sh explore                          # serverless free-roam, no login
```

`play.sh` connects to `WOW_HOST`, defaulting to `127.0.0.1`. Its `WOW_USER` and `WOW_PASS`
defaults are the placeholders `<username>` and `<password>`, so set both to your own account.

The variables the client reads at startup:

| Variable | What it does |
| --- | --- |
| `WOW_DATA` | The install's `Data` folder. Set and empty (`WOW_DATA=`) means "no install", and the client refuses to start. |
| `WOW_HOST` | The realm server, `host` or `host:port`; default `localhost:3724`. The Realmlist button on the login screen sets it too and remembers it. Ignored for the logon when the tunnel is on (§7). |
| `WOW_USER`, `WOW_PASS` | Both set: the login screen submits them by itself. Either missing: you type them in. |
| `BENILLA_HOME` | The state folder: `config.toml`, bindings, macros, saved variables, `AddOns/`. Default `benilla-config/` at the repo root for a dev build, beside the binary for a player build. |
| `WOW_UNATTENDED=1` | Reconnects instead of waiting at a dialog and exits non-zero on a login it cannot pass. Such a run also starts from a fresh state folder in the system temp folder unless `BENILLA_HOME` is set. |
| `WOW_NOSOUND=1` | Runs without sound. |
| `BENILLA_TUNNEL_CONFIG` | The tunnel file (§7). |

A dev build started from a linked git worktree shares the main checkout's `benilla-config/`,
unless the worktree has its own `.probe-identity` file.

## 4. Pre-accepting the EULA and TOS (`config.toml`)

On a first launch the login screen shows the Terms of Use and the EULA before the account boxes,
and logging in is blocked until both are accepted. The screen reads the `readTOS` and `readEULA`
settings at startup; with both set to `1` it goes straight to the account boxes.
`readScanning` and `readContest` are the other two notices of the same kind.

Settings live in `config.toml` in the state folder (§3), as a `[cvars]` table of
`Name = "value"` strings. If the file does not exist yet, create it:

```sh
mkdir -p benilla-config
cat > benilla-config/config.toml <<'EOF'
[cvars]
readContest = "1"
readEULA = "1"
readScanning = "1"
readTOS = "1"
EOF
```

```powershell
New-Item -ItemType Directory -Force benilla-config | Out-Null
@'
[cvars]
readContest = "1"
readEULA = "1"
readScanning = "1"
readTOS = "1"
'@ | Set-Content -Encoding ascii benilla-config\config.toml
```

If `config.toml` already exists, add the four lines under its `[cvars]` header instead of
replacing the file, or your other settings are lost. With `BENILLA_HOME` set, the file goes in
that folder; an unattended run without `BENILLA_HOME` starts from defaults and reads neither.
A `config.toml` the client cannot parse is left as it is and never saved over.

## 5. Running on Windows (PowerShell)

PowerShell sets a variable for the session with `$env:`; it stays set for later commands in the
same window.

```powershell
$env:WOW_DATA = "C:\path\to\WoW\Data"
cargo play
```

Or with a `WoW` junction at the repo root, which needs no admin rights:

```powershell
New-Item -ItemType Junction -Path WoW -Target C:\path\to\WoW
cargo play
```

A connected run with the credentials and server given up front, the `play.sh` equivalent:

```powershell
$env:WOW_DATA = "C:\path\to\WoW\Data"
$env:WOW_HOST = "127.0.0.1"
$env:WOW_USER = "<account>"
$env:WOW_PASS = "<password>"
.\target\play\benilla.exe
```

Explore mode, with no server:

```powershell
$env:WOW_DATA = "C:\path\to\WoW\Data"
cargo explore
```

To clear a variable again: `Remove-Item Env:WOW_PASS`.

## 6. `.watch` and other server commands

benilla has no `.watch` command of its own: no file in this repository handles one. A chat line
that starts with `.` (or `!`) goes to the server as ordinary chat, and the server runs it as a
command if the account's GM level allows it. So `.watch` and its subcommands are whatever the
server you connect to defines; ask that server for its list (MaNGOS-family cores answer `.help`
with the commands your account may use).

What the client does with such a line:

- It sends it as typed; it does not parse or check it.
- The wire trace (`WOW_MOVE_TRACE` with the tag `out`) records it as `hex=<redacted>`, as it does
  every chat line starting with `.` or `!`, so a trace can be shared without the commands in it.
- `WOW_PROBE_CHAT="<command>"` sends one such command from a scripted run and logs the reply as
  `net: server says` ([`CONTRIBUTING.md`](CONTRIBUTING.md), "Running it unattended").

## 7. Connecting over the `wss` tunnel

When the realm is reachable only through the `warcraft-tunnel` bridge, the client sends the
logon and the world connection through a WebSocket instead of direct TCP. Nothing else about
the protocol changes. A tunnel file turns it on; with no file the client connects directly.

The file, `tunnel.toml` (it holds a secret: keep it out of any repository and readable only by
you):

```toml
endpoint  = "wss://example.com"   # or ws://127.0.0.1:9090 for a local bridge
base_path = "/<path>"             # the bridge's path; may be empty, else starts with /
key       = "<key>"               # your member key
```

The client takes the first of these that is set
(`crates/benilla-protocol/src/tunnel_config.rs`):

1. `BENILLA_TUNNEL_CONFIG`: the path of the file.
2. `ARMY_TUNNEL_CONFIG`: the older name of the same variable, used by `warcraft-army`.
3. `BENILLA_TUNNEL_URL` and `BENILLA_TUNNEL_KEY`, plus the optional `BENILLA_TUNNEL_PATH`: the
   same three values as variables, with no file.
4. The default file: `$XDG_CONFIG_HOME/benilla/tunnel.toml`, else
   `~/.config/benilla/tunnel.toml`; on Windows `%APPDATA%\benilla\tunnel.toml`.

```sh
mkdir -p ~/.config/benilla
$EDITOR ~/.config/benilla/tunnel.toml
chmod 600 ~/.config/benilla/tunnel.toml
WOW_DATA=/path/to/WoW/Data cargo play

# or name the file for one run
BENILLA_TUNNEL_CONFIG=/path/to/tunnel.toml WOW_DATA=/path/to/WoW/Data cargo play

# or the variables, no file
BENILLA_TUNNEL_URL=wss://example.com BENILLA_TUNNEL_PATH=/<path> BENILLA_TUNNEL_KEY=<key> \
  WOW_DATA=/path/to/WoW/Data cargo play
```

```powershell
New-Item -ItemType Directory -Force "$env:APPDATA\benilla" | Out-Null
notepad "$env:APPDATA\benilla\tunnel.toml"
cargo play

# or name the file for this window
$env:BENILLA_TUNNEL_CONFIG = "C:\path\to\tunnel.toml"
cargo play

# or the variables, no file
$env:BENILLA_TUNNEL_URL = "wss://example.com"
$env:BENILLA_TUNNEL_PATH = "/<path>"
$env:BENILLA_TUNNEL_KEY = "<key>"
cargo play
```

The client opens `<endpoint><base_path>/realm` for the logon and `<endpoint><base_path>/world`
for the world, and presents the key as `Authorization: Bearer <key>`. On the first connect it
prints one line naming the endpoint and where the settings came from, never the key or the path:

```
tunnel: connecting through wss://example.com (settings from /home/me/.config/benilla/tunnel.toml)
```

No such line means the client found no tunnel settings and is connecting directly to
`WOW_HOST`.

## 8. Login and connection troubleshooting

Start the client from a terminal: the lines below go to its output. The first line of the
output, `benilla build ...`, is the one to quote in a bug report.

| What you see | Why | What to do |
| --- | --- | --- |
| A dialog or `benilla: benilla needs a World of Warcraft 1.12.1 install and found none.` with the folders it looked in | No install found, or `WOW_DATA` is set and empty | Set `WOW_DATA` to the install's `Data` folder, or make the `WoW` link (§3, §5). |
| `benilla: the WoW install cannot be read` | The `Data` folder's archives do not open | Point `WOW_DATA` at an unmodified 1.12.1 `Data` folder. |
| The EULA or TOS screen every time, and the login button does nothing | The agreements are not accepted in this state folder | Accept them once, or pre-accept them (§4). An unattended run without `BENILLA_HOME` starts from defaults each time. |
| Over the tunnel, no `tunnel: connecting through ...` line | No tunnel settings found; the client connects directly to `WOW_HOST` | Check the file location or the variables (§7). |
| `tunnel: BENILLA_TUNNEL_CONFIG = <path>: <error>; using direct TCP` (or `tunnel: <path>: <error>; ...`) | The named file is missing or invalid: `endpoint must be a ws:// or wss:// URL`, `key is missing`, `base_path must start with /` | Fix the file; the client falls back to direct TCP until then. |
| `tunnel: <path> holds a secret key but is readable by others; chmod 600 it` | The file's permissions | `chmod 600 <path>`. The connect still goes ahead. |
| `connecting to the tunnel at <host>:<port>`, or `tunnel TLS handshake failed` | The bridge is not reachable from here, or TLS fails | Check `endpoint`, the network and any proxy. |
| `tunnel handshake failed: ...` with HTTP 401 | The bridge refused the key | Check `key`, and that it has not been revoked. |
| `tunnel handshake failed: ...` with another HTTP status | The URL does not reach the bridge's `/realm` or `/world` | Check `base_path`. |
| `login: refused (...)` | realmd or the world server refused the account | Check the account name and password, and that the account exists on that server. |
| `login: REFUSING the env fast path — ...` | `WOW_USER` differs from the account this checkout's `.probe-identity` declares | Use the declared account, or set `WOW_ALLOW_ACCOUNT=1` when the other account is deliberate. |
| `net: disconnected: world stream closed: ...`, then `session over, back to the login screen` | The server or the bridge closed the connection, for example a realm restart | Log in again once the server is back. |
| Disconnected a fixed time after entering the world | The server enforces Warden, which benilla does not answer | Use a server with Warden off (vmangos ships with it off). |
| `connecting: ...` (for example `Connection refused`) or `resolving <host>: ...` on a direct connect | Nothing is listening at `WOW_HOST`, the port is blocked, or the name does not resolve | Check the server is up, and the host and port (default `3724`). |
| A PIN-protected account fails over the tunnel | A tunnelled logon carries no PIN keypad | Use an account without a PIN over the tunnel. |

## 9. Checking a change

- `cargo test -p <crate>` runs one crate's tests.
- `scripts/check.sh` checks the crates your branch changed against `main`, and their
  dependents.
- `scripts/gates.sh` is the full chain: fmt, clippy with warnings denied, the workspace tests,
  the lints, the player build and the engine boot checks. It opens a window, so it needs a
  display.
- `scripts/smoke.sh` is the live login gate. It needs `WOW_USER`, `WOW_PASS` and `WOW_CHAR`, in
  the environment or in a `.probe-identity` file at the repo root, one per line.

[`CONTRIBUTING.md`](CONTRIBUTING.md), "Setting up", has the rest.
