//! The session manager: one background worker thread per account, so a blocking SRP6 login never
//! freezes the UI. The UI calls [`Manager::login`]/[`Manager::logout`]/[`Manager::create`] and
//! reads [`Manager::status`] + [`Manager::notices`], which [`Manager::pump`] refreshes each frame
//! from the workers' updates.
//!
//! Accounts are hardcoded here for now (the two test bots). The next increment swaps this for
//! vmangos-DB discovery plus an `army-secrets.toml` for the login passwords (the DB holds only
//! SRP6 verifiers, never plaintext, so passwords must come from the local file).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use benilla_protocol::{EntityKind, SessionEvent};

use crate::session::{Account, CharInfo, NewChar, Session};
use crate::terrain::Terrain;

/// Movement-packet cadence and the min interval between movement ticks.
const TICK: Duration = Duration::from_millis(100);
/// Ground run speed (yd/s), the stock `MOVE_RUN`; the bot advances at most this per tick.
const RUN_SPEED: f32 = 7.0;
/// Horizontal distance (yd) at which the bot counts a move-to goal (or path endpoint) as reached.
const ARRIVE_EPS: f32 = 0.75;
/// Horizontal distance (yd) at which an *intermediate* path waypoint counts as reached (looser than
/// the final endpoint so dead-reckoning steps don't miss a corner and stall).
const WAYPOINT_EPS: f32 = 1.5;
/// How close the bot trails a followed player (yd) before it stops and waits.
const FOLLOW_STOP: f32 = 4.0;
/// A path walk with no forward progress (toward the current waypoint) for this long is declared
/// `Blocked` — a mesh gap, a wall the straight segment can't cross, or a body in the way.
const STUCK_SECS: f32 = 6.0;
/// Minimum horizontal progress (yd) that resets the stuck timer.
const STUCK_EPS: f32 = 0.5;
/// How far (yd) the precise navmesh surface must sit above/below the ADT terrain before we treat it
/// as a STRUCTURE (a building floor/stairs the heightmap lacks) and stand on the navmesh surface
/// instead of the ground. Small, because the comparison uses Detour's exact `getPolyHeight` (which
/// matches the real ground within inches on open terrain), so even a low floor is caught.
const FLOOR_EPS: f32 = 0.7;
/// If the navmesh's reachable endpoint is farther than this (yd) from what the agent requested, the
/// plan is reported `partial` (the query capped short — see the node-budget behaviour).
const PARTIAL_EPS: f32 = 10.0;

/// The synchronous result of accepting a `move_to`: the honest plan, before the walk runs. `partial`
/// means the navmesh could only route to `endpoint`, short of `requested`; the agent then polls the
/// bot's `MoveStatus` (via `list_bots`) to learn when it actually arrives (or is blocked).
pub struct MovePlan {
    pub accepted: bool,
    pub move_id: u64,
    pub requested: [f32; 3],
    /// The committed reachable endpoint (== requested when complete; a capped point when partial).
    pub endpoint: [f32; 3],
    pub partial: bool,
    pub waypoints: usize,
    /// Path length in yards (sum of horizontal segment lengths).
    pub distance: f32,
    /// Set only when `accepted` is false.
    pub reason: Option<String>,
}

impl MovePlan {
    fn rejected(requested: [f32; 3], reason: &str) -> Self {
        Self {
            accepted: false,
            move_id: 0,
            requested,
            endpoint: requested,
            partial: false,
            waypoints: 0,
            distance: 0.0,
            reason: Some(reason.to_string()),
        }
    }
}

/// Sum of horizontal segment lengths of a waypoint list.
fn path_length(wps: &[[f32; 3]]) -> f32 {
    wps.windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum()
}

/// A bot account's live state, as the UI renders it.
#[derive(Clone)]
pub enum Status {
    Offline,
    /// Login in flight on a worker thread.
    Connecting,
    /// Authenticated and parked at character select.
    Online {
        roster: Vec<CharInfo>,
        in_world: Option<u64>,
    },
    /// The last login attempt failed; the string is the error chain.
    Error(String),
}

impl Status {
    pub fn is_online(&self) -> bool {
        matches!(self, Status::Online { .. })
    }
    pub fn is_connecting(&self) -> bool {
        matches!(self, Status::Connecting)
    }
}

/// A worker's report to the UI.
enum Update {
    /// A new lifecycle state for the account.
    Status { user: String, status: Status },
    /// A transient message (e.g. a failed create) that must not change the lifecycle state.
    Notice { user: String, text: String },
    /// The in-world bot's current position, whether it's walking, and the map it's on (the map is
    /// what the control side needs to pick the right navmesh when planning a MoveTo).
    Pose {
        user: String,
        pos: [f32; 3],
        moving: bool,
        map: u32,
    },
    /// The in-world bot's path-movement status, for the agent to poll (see [`MoveStatus`]).
    Move { user: String, status: MoveStatus },
}

/// Where a path walk is, as the agent polls it. A MoveTo is accepted synchronously (with a plan),
/// then the bot walks asynchronously; this is how the agent learns it truly `Arrived` (vs. `Blocked`
/// short of the endpoint). `move_id` correlates the status with the accept-time plan.
#[derive(Clone, Copy, PartialEq)]
pub enum MoveState {
    Idle,
    Moving,
    Arrived,
    Blocked,
}

/// A bot's live path-movement status.
#[derive(Clone, Copy)]
pub struct MoveStatus {
    pub state: MoveState,
    /// The committed endpoint the walk is heading to (the plan's reachable point).
    pub goal: Option<[f32; 3]>,
    /// Horizontal yards still to the endpoint.
    pub remaining: f32,
    /// Correlates with the `move_id` returned by the accepting `move_to`.
    pub move_id: u64,
}

impl Default for MoveStatus {
    fn default() -> Self {
        Self { state: MoveState::Idle, goal: None, remaining: 0.0, move_id: 0 }
    }
}

/// A command from the UI to a running worker.
enum Cmd {
    Create(NewChar),
    /// Enter the world as this character guid.
    Enter(u64),
    /// Walk to this WoW-coords point in a straight line (no obstacle avoidance) — the UI/raw path.
    MoveTo([f32; 3]),
    /// Walk an ordered navmesh route: the waypoints (string-pulled corners) to the committed
    /// `endpoint`, tagged with `move_id` for status correlation. The control side computes this from
    /// the navmesh; the worker just walks it and reports `MoveStatus`.
    Walk { waypoints: Vec<[f32; 3]>, endpoint: [f32; 3], move_id: u64 },
    /// Claim an EXACT position with NO ground-clamp — fly/sink allowed. The adversarial / anti-cheat
    /// -test primitive the MCP `raw_move` tool sends; the server (with anti-cheat) adjudicates it.
    RawMove([f32; 3]),
    /// Follow the nearest player, trailing a few yards behind.
    Follow,
    /// Stop walking / following where you are.
    Stop,
    /// Leave the world back to character select, staying logged in.
    Leave,
    Logout,
}

/// A lifecycle request from the MCP server (off the UI thread) that needs the `&mut Manager` — so
/// it can't go through the shared command map. Drained by [`Manager::pump`] on the UI thread.
enum ControlReq {
    /// Log the account in (spawn its worker).
    Login(String),
    /// Enter the world; `None` = the account's first roster character.
    Enter(String, Option<u64>),
    /// Log the account out.
    Logout(String),
    /// Register a new account's credentials at runtime (no restart), so it can then be logged in.
    AddAccount(Account),
}

/// A read-only snapshot of one bot, shared with the MCP server so an agent can see lifecycle state,
/// roster, and live pose without touching the UI thread. Rebuilt each [`Manager::pump`].
#[derive(Clone)]
pub struct BotSnapshot {
    /// "offline" | "connecting" | "char_select" | "in_world" | "error: …".
    pub state: String,
    /// (guid, name, level) for each character, when known.
    pub roster: Vec<(u64, String, u8)>,
    /// The guid currently in world, if any.
    pub in_world: Option<u64>,
    /// Latest in-world position, if reported.
    pub pos: Option<[f32; 3]>,
    /// Whether the bot is walking.
    pub moving: bool,
    /// The map the in-world bot is on (0 until it reports a pose). The control side needs it to pick
    /// the navmesh when planning a MoveTo.
    pub map: u32,
    /// Latest path-movement status (for the agent to poll a MoveTo to completion).
    pub move_status: MoveStatus,
}

/// How an in-world loop ended.
enum InWorldExit {
    /// Back to character select, session still alive.
    CharSelect,
    /// The session is finished (clean logout, or a drop carrying its reason).
    Dropped(Option<String>),
}

/// The accounts the army may drive, loaded from a PRIVATE `army-secrets.toml` — never from the
/// repo (the passwords are credentials; see [`Account`]). The file is named by `ARMY_SECRETS`, or
/// `army-secrets.toml` in the working directory by default. When it is absent the army simply
/// starts with an empty roster, so no credentials ever need to live in the tree. See
/// `army-secrets.toml.example` for the format.
pub fn default_accounts() -> Vec<Account> {
    let path = std::env::var("ARMY_SECRETS").unwrap_or_else(|_| "army-secrets.toml".into());
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_accounts(&text),
        Err(_) => {
            eprintln!(
                "army: no accounts file at {path} (set ARMY_SECRETS, or copy \
                 army-secrets.toml.example to army-secrets.toml); starting with an empty roster"
            );
            Vec::new()
        }
    }
}

/// Parse the roster file — a tiny TOML of `[[account]]` tables (`user`/`pass`, optional per-account
/// `realmlist`), with an optional top-level `realmlist` default for all of them. Hand-parsed to
/// avoid a TOML dependency in the army, matching `session::tunnel_config`. Accounts missing a
/// `user` or `pass` are skipped.
fn parse_accounts(text: &str) -> Vec<Account> {
    let mut default_realmlist = "127.0.0.1".to_string();
    let mut accounts: Vec<Account> = Vec::new();
    let mut cur: Option<Account> = None;
    let mut seen_table = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[account]]" {
            if let Some(a) = cur.take() {
                if !a.user.is_empty() && !a.pass.is_empty() {
                    accounts.push(a);
                }
            }
            cur = Some(Account {
                user: String::new(),
                pass: String::new(),
                realmlist: default_realmlist.clone(),
            });
            seen_table = true;
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match (cur.as_mut(), k.trim()) {
                (Some(a), "user") => a.user = v,
                (Some(a), "pass") => a.pass = v,
                (Some(a), "realmlist") => a.realmlist = v,
                // A top-level `realmlist` (before any table) is the default for every account.
                (None, "realmlist") if !seen_table => default_realmlist = v,
                _ => {}
            }
        }
    }
    if let Some(a) = cur.take() {
        if !a.user.is_empty() && !a.pass.is_empty() {
            accounts.push(a);
        }
    }
    accounts
}

pub struct Manager {
    pub accounts: Vec<Account>,
    pub status: HashMap<String, Status>,
    /// Latest transient message per account (create refusals, etc.).
    pub notices: HashMap<String, String>,
    /// Latest in-world pose per account: `(position, moving)`.
    poses: HashMap<String, ([f32; 3], bool)>,
    /// Latest reported map per account (for navmesh selection on the control side).
    maps: HashMap<String, u32>,
    /// Latest path-movement status per account.
    moves: HashMap<String, MoveStatus>,
    updates_rx: Receiver<Update>,
    updates_tx: Sender<Update>,
    workers: HashMap<String, Sender<Cmd>>,
    /// A clone of each live worker's command sender, shared with the MCP server thread so an agent
    /// can drive a logged-in bot. Mirrors `workers`; synced on login and on worker exit.
    control: Arc<Mutex<HashMap<String, Sender<Cmd>>>>,
    /// Lifecycle requests from the MCP server (login/enter/logout) that need `&mut Manager`.
    req_tx: Sender<ControlReq>,
    req_rx: Receiver<ControlReq>,
    /// Read-only per-account snapshot shared with the MCP server. Rebuilt each `pump`.
    snap: Arc<Mutex<HashMap<String, BotSnapshot>>>,
    /// Monotonic move-id source, shared with control handles so each accepted MoveTo gets a unique
    /// id the agent can correlate with the polled `MoveStatus`.
    move_seq: Arc<AtomicU64>,
}

impl Manager {
    pub fn new(accounts: Vec<Account>) -> Self {
        let (updates_tx, updates_rx) = channel();
        let (req_tx, req_rx) = channel();
        let status: HashMap<String, Status> = accounts
            .iter()
            .map(|a| (a.user.clone(), Status::Offline))
            .collect();
        // Seed the shared snapshot so the MCP server can list accounts before any login.
        let snap = accounts
            .iter()
            .map(|a| {
                (
                    a.user.clone(),
                    BotSnapshot {
                        state: "offline".to_string(),
                        roster: Vec::new(),
                        in_world: None,
                        pos: None,
                        moving: false,
                        map: 0,
                        move_status: MoveStatus::default(),
                    },
                )
            })
            .collect();
        Self {
            accounts,
            status,
            notices: HashMap::new(),
            poses: HashMap::new(),
            maps: HashMap::new(),
            moves: HashMap::new(),
            updates_rx,
            updates_tx,
            workers: HashMap::new(),
            control: Arc::new(Mutex::new(HashMap::new())),
            req_tx,
            req_rx,
            snap: Arc::new(Mutex::new(snap)),
            move_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A clone-able handle the MCP server uses to drive bots off the UI thread.
    pub fn control_handle(&self) -> ControlHandle {
        ControlHandle {
            control: self.control.clone(),
            req_tx: self.req_tx.clone(),
            snap: self.snap.clone(),
            move_seq: self.move_seq.clone(),
        }
    }

    /// The account's first roster character guid, if it's online with a roster.
    fn first_guid(&self, user: &str) -> Option<u64> {
        match self.status_of(user) {
            Status::Online { roster, .. } => roster.first().map(|c| c.guid),
            _ => None,
        }
    }

    /// Rebuild the shared MCP snapshot from current status + poses.
    fn refresh_snap(&self) {
        let mut snap = self.snap.lock().unwrap();
        for acct in &self.accounts {
            let user = &acct.user;
            let (state, roster, in_world) = match self.status_of(user) {
                Status::Offline => ("offline".to_string(), Vec::new(), None),
                Status::Connecting => ("connecting".to_string(), Vec::new(), None),
                Status::Error(e) => (format!("error: {e}"), Vec::new(), None),
                Status::Online { roster, in_world } => {
                    let state = if in_world.is_some() { "in_world" } else { "char_select" };
                    let r = roster.iter().map(|c| (c.guid, c.name.clone(), c.level)).collect();
                    (state.to_string(), r, *in_world)
                }
            };
            let (pos, moving) = match self.poses.get(user) {
                Some((p, m)) => (Some(*p), *m),
                None => (None, false),
            };
            let map = self.maps.get(user).copied().unwrap_or(0);
            let move_status = self.moves.get(user).copied().unwrap_or_default();
            snap.insert(
                user.clone(),
                BotSnapshot { state, roster, in_world, pos, moving, map, move_status },
            );
        }
    }

    /// The in-world bot's latest `(position, moving)`, if it has reported one.
    pub fn pose_of(&self, user: &str) -> Option<([f32; 3], bool)> {
        self.poses.get(user).copied()
    }

    /// Walk the in-world bot to a WoW-coords point.
    pub fn move_to(&mut self, user: &str, point: [f32; 3]) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::MoveTo(point));
        }
    }

    /// Make the in-world bot follow the nearest player.
    pub fn follow(&mut self, user: &str) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::Follow);
        }
    }

    /// Stop the in-world bot where it is.
    pub fn stop_moving(&mut self, user: &str) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::Stop);
        }
    }

    pub fn status_of(&self, user: &str) -> &Status {
        self.status.get(user).unwrap_or(&Status::Offline)
    }

    pub fn notice_of(&self, user: &str) -> Option<&str> {
        self.notices.get(user).map(String::as_str)
    }

    /// Start a login for `user` on its own worker thread; the UI sees `Connecting` at once.
    pub fn login(&mut self, user: &str) {
        if self.workers.contains_key(user) {
            return; // already running
        }
        let Some(acct) = self.accounts.iter().find(|a| a.user == user).cloned() else {
            return;
        };
        self.status.insert(user.to_string(), Status::Connecting);
        self.notices.remove(user);

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        self.control
            .lock()
            .unwrap()
            .insert(user.to_string(), cmd_tx.clone());
        self.workers.insert(user.to_string(), cmd_tx);
        let updates = self.updates_tx.clone();
        thread::spawn(move || worker(acct, cmd_rx, updates));
    }

    /// Ask `user`'s worker to log out and exit.
    pub fn logout(&mut self, user: &str) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::Logout);
        }
    }

    /// Ask `user`'s (online) worker to create a character.
    pub fn create(&mut self, user: &str, spec: NewChar) {
        if let Some(tx) = self.workers.get(user) {
            self.notices.remove(user);
            let _ = tx.send(Cmd::Create(spec));
        }
    }

    /// Register an account at runtime (no restart). No-op if the username already exists. The next
    /// `refresh_snap()` in `pump` adds it to the shared snapshot so the MCP server sees it.
    fn add_account(&mut self, acct: Account) {
        if self.accounts.iter().any(|a| a.user == acct.user) {
            return;
        }
        self.status.insert(acct.user.clone(), Status::Offline);
        self.accounts.push(acct);
    }

    /// Ask `user`'s worker to enter the world as `guid`.
    pub fn enter(&mut self, user: &str, guid: u64) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::Enter(guid));
        }
    }

    /// Ask `user`'s worker to leave the world (back to character select, still logged in).
    pub fn leave(&mut self, user: &str) {
        if let Some(tx) = self.workers.get(user) {
            let _ = tx.send(Cmd::Leave);
        }
    }

    /// Drain worker reports into `status`/`notices`; returns true while any account is still
    /// connecting (so the UI knows to keep repainting).
    pub fn pump(&mut self) -> bool {
        // Lifecycle requests from the MCP server (need &mut self, so they funnel through here).
        while let Ok(req) = self.req_rx.try_recv() {
            match req {
                ControlReq::Login(user) => self.login(&user),
                ControlReq::Enter(user, guid) => {
                    if let Some(g) = guid.or_else(|| self.first_guid(&user)) {
                        self.enter(&user, g);
                    }
                }
                ControlReq::Logout(user) => self.logout(&user),
                ControlReq::AddAccount(acct) => self.add_account(acct),
            }
        }
        while let Ok(update) = self.updates_rx.try_recv() {
            match update {
                Update::Status { user, status } => {
                    if matches!(status, Status::Offline | Status::Error(_)) {
                        self.workers.remove(&user);
                        self.control.lock().unwrap().remove(&user);
                    }
                    self.status.insert(user, status);
                }
                Update::Notice { user, text } => {
                    self.notices.insert(user, text);
                }
                Update::Pose { user, pos, moving, map } => {
                    self.maps.insert(user.clone(), map);
                    self.poses.insert(user, (pos, moving));
                }
                Update::Move { user, status } => {
                    self.moves.insert(user, status);
                }
            }
        }
        self.refresh_snap();
        // Repaint while any worker is live: connecting accounts flip soon, and in-world ones can
        // change state (e.g. a dropped connection) without a UI input to trigger a frame.
        self.status
            .values()
            .any(|s| s.is_connecting() || s.is_online())
    }
}

/// Off-UI-thread control of logged-in bots, for the MCP server (`mcp.rs`). Holds a shared clone of
/// the worker command senders; each call looks up the account `user` and sends a `Cmd`.
#[derive(Clone)]
pub struct ControlHandle {
    control: Arc<Mutex<HashMap<String, Sender<Cmd>>>>,
    req_tx: Sender<ControlReq>,
    snap: Arc<Mutex<HashMap<String, BotSnapshot>>>,
    move_seq: Arc<AtomicU64>,
}

impl ControlHandle {
    /// Account `user`s that have a live worker (logged in, char-select or in world).
    pub fn users(&self) -> Vec<String> {
        let mut v: Vec<String> = self.control.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    /// A snapshot of every known account (lifecycle, roster, pose), sorted by name.
    pub fn snapshot(&self) -> Vec<(String, BotSnapshot)> {
        let mut v: Vec<(String, BotSnapshot)> = self
            .snap
            .lock()
            .unwrap()
            .iter()
            .map(|(u, s)| (u.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Log the account in (async; poll `snapshot` for the result).
    pub fn login(&self, user: &str) -> bool {
        self.req_tx.send(ControlReq::Login(user.to_string())).is_ok()
    }

    /// Register an account's credentials in the running adapter (no restart); then it can be logged in.
    pub fn add_account(&self, user: &str, pass: &str, realmlist: &str) -> bool {
        self.req_tx
            .send(ControlReq::AddAccount(Account {
                user: user.to_string(),
                pass: pass.to_string(),
                realmlist: realmlist.to_string(),
            }))
            .is_ok()
    }

    /// Enter the world as `guid` (or the first roster character when `None`).
    pub fn enter(&self, user: &str, guid: Option<u64>) -> bool {
        self.req_tx.send(ControlReq::Enter(user.to_string(), guid)).is_ok()
    }

    /// Log the account out.
    pub fn logout(&self, user: &str) -> bool {
        self.req_tx.send(ControlReq::Logout(user.to_string())).is_ok()
    }

    /// Claim an EXACT position (no ground-clamp). Returns false if `user` has no live worker.
    pub fn raw_move(&self, user: &str, pos: [f32; 3]) -> bool {
        self.send(user, Cmd::RawMove(pos))
    }

    /// Plan and start a navmesh walk to `dest` (WoW coords), returning the honest [`MovePlan`]
    /// synchronously. Pathfinding happens here on the control side (so we can answer the agent at
    /// once); the worker just walks the resulting waypoints and reports `MoveStatus` for the agent to
    /// poll. A `partial` plan means the navmesh could only route part-way — the agent decides what to
    /// do. Rejected if the bot isn't in world, its position/map is unknown, or there's no path.
    pub fn move_to(&self, user: &str, dest: [f32; 3]) -> MovePlan {
        let (pos, map, in_world) = {
            let snap = self.snap.lock().unwrap();
            match snap.get(user) {
                Some(s) => (s.pos, s.map, s.in_world.is_some()),
                None => (None, 0, false),
            }
        };
        if !in_world {
            return MovePlan::rejected(dest, "not in world (login + enter_world first)");
        }
        let Some(start) = pos else {
            return MovePlan::rejected(dest, "position not reported yet (try again in a moment)");
        };
        let Some(nav) = crate::navdt::shared(map) else {
            return MovePlan::rejected(dest, &format!("no navmesh available for map {map}"));
        };
        let path = nav.lock().unwrap().find_path(start, dest);
        let Some(wps) = path else {
            return MovePlan::rejected(dest, "no path (start or destination off the navmesh, or disconnected)");
        };
        let Some(&endpoint) = wps.last() else {
            return MovePlan::rejected(dest, "empty path");
        };
        let partial = (endpoint[0] - dest[0]).hypot(endpoint[1] - dest[1]) > PARTIAL_EPS;
        let distance = path_length(&wps);
        let move_id = self.move_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.send(user, Cmd::Walk { waypoints: wps.clone(), endpoint, move_id }) {
            return MovePlan::rejected(dest, "no live worker");
        }
        MovePlan {
            accepted: true,
            move_id,
            requested: dest,
            endpoint,
            partial,
            waypoints: wps.len(),
            distance,
            reason: None,
        }
    }

    /// Create a character (default all-zero look) on a logged-in, char-select account. `race`/`class`
    /// are raw `ChrRaces`/`ChrClasses` ids (Human=1; Warrior=1, Paladin=2, Rogue=4, Priest=5, Mage=8),
    /// `gender` is 0=male/1=female. The server validates the race/class/gender combo and the all-zero
    /// appearance against its create catalog. Use before enter_world when the account has no character
    /// yet. Returns false if `user` has no live worker.
    pub fn create_char(&self, user: &str, name: &str, race: u8, class: u8, gender: u8) -> bool {
        self.send(user, Cmd::Create(NewChar {
            name: name.to_string(),
            race,
            class,
            gender,
            skin: 0,
            face: 0,
            hair_style: 0,
            hair_color: 0,
            facial_hair: 0,
        }))
    }

    fn send(&self, user: &str, cmd: Cmd) -> bool {
        self.control
            .lock()
            .unwrap()
            .get(user)
            .is_some_and(|tx| tx.send(cmd).is_ok())
    }
}

/// One account's lifetime: log in, report the roster, then serve commands (create / logout) until
/// the UI asks it to log out (or the channel closes on app exit).
fn worker(acct: Account, cmd_rx: Receiver<Cmd>, updates: Sender<Update>) {
    let user = acct.user.clone();
    let status = |s: Status| {
        let _ = updates.send(Update::Status {
            user: user.clone(),
            status: s,
        });
    };
    let notice = |t: String| {
        let _ = updates.send(Update::Notice {
            user: user.clone(),
            text: t,
        });
    };

    let mut session = match Session::login(&acct) {
        Ok(s) => s,
        Err(e) => {
            status(Status::Error(format!("{e:#}")));
            return;
        }
    };
    let online = |s: &Session| Status::Online {
        roster: s.roster.clone(),
        in_world: s.in_world,
    };
    status(online(&session));

    // Character-select command loop. Blocking here is fine: vmangos tolerates a quiet authenticated
    // socket parked at select. Entering the world switches to the keepalive loop below.
    loop {
        match cmd_rx.recv() {
            Ok(Cmd::Create(spec)) => match session.create(&spec) {
                Ok(()) => status(online(&session)),
                Err(e) => notice(format!("{e:#}")),
            },
            Ok(Cmd::Enter(guid)) => {
                if let Err(e) = session.enter(guid) {
                    notice(format!("{e:#}"));
                    continue;
                }
                status(online(&session)); // now Online { in_world: Some(guid) }
                // Dead-reckon from the character's saved spawn position, on its map's terrain.
                let (start, map_id) = session
                    .roster
                    .iter()
                    .find(|c| c.guid == guid)
                    .map_or(([0.0; 3], 0), |c| (c.pos, c.map));
                let mut terrain = Terrain::open(map_id);
                match run_in_world(
                    &mut session, &cmd_rx, &updates, &user, guid, start, &mut terrain, map_id,
                ) {
                    InWorldExit::CharSelect => status(online(&session)),
                    InWorldExit::Dropped(reason) => {
                        if let Some(r) = reason {
                            notice(r);
                        }
                        drop(session);
                        status(Status::Offline);
                        return;
                    }
                }
            }
            // Only meaningful in the world; ignored while parked at character select.
            Ok(Cmd::Leave) | Ok(Cmd::MoveTo(_)) | Ok(Cmd::Walk { .. }) | Ok(Cmd::RawMove(_))
            | Ok(Cmd::Follow) | Ok(Cmd::Stop) => {}
            Ok(Cmd::Logout) | Err(_) => {
                drop(session); // close the socket; the server logs the character out
                status(Status::Offline);
                return;
            }
        }
    }
}

/// Snap a position's Z onto the terrain, if a height is available there.
fn clamp_z(pos: &mut [f32; 3], terrain: &mut Option<Terrain>) {
    if let Some(gz) = terrain.as_mut().and_then(|t| t.height(pos[0], pos[1])) {
        pos[2] = gz;
    }
}

/// What the in-world loop is pursuing.
enum Goal {
    Idle,
    Point([f32; 3]),
    /// An ordered navmesh route: remaining waypoints (front = next), the committed endpoint, and the
    /// move id for status correlation.
    Path {
        wps: std::collections::VecDeque<[f32; 3]>,
        endpoint: [f32; 3],
        move_id: u64,
    },
    FollowNearest,
}

/// Update the nearby-player tracker from one decoded event (our own body excluded).
fn track(players: &mut HashMap<u64, [f32; 3]>, self_guid: u64, ev: &SessionEvent) {
    match ev {
        SessionEvent::ObjectCreate {
            guid,
            kind,
            position,
            ..
        } if *kind == EntityKind::Player && *guid != self_guid => {
            players.insert(*guid, *position);
        }
        SessionEvent::UnitMove { guid, position, .. } if players.contains_key(guid) => {
            players.insert(*guid, *position);
        }
        SessionEvent::Teleport { guid, position, .. } if players.contains_key(guid) => {
            players.insert(*guid, *position);
        }
        SessionEvent::ObjectDestroyed(guid) => {
            players.remove(guid);
        }
        SessionEvent::ObjectsRemoved(guids) => {
            for g in guids {
                players.remove(g);
            }
        }
        _ => {}
    }
}

/// The nearest tracked player's position to `pos`, by horizontal distance.
fn nearest_player(players: &HashMap<u64, [f32; 3]>, pos: [f32; 3]) -> Option<[f32; 3]> {
    players.values().copied().min_by(|a, b| {
        let da = (a[0] - pos[0]).hypot(a[1] - pos[1]);
        let db = (b[0] - pos[0]).hypot(b[1] - pos[1]);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Hold an in-world session alive and drive its movement. It reads+decodes the inbound stream into
/// a nearby-player tracker, advances toward the active goal (a point, or the nearest player) at run
/// speed while ground-clamping Z, streams `MSG_MOVE_*` so observers see it run, reports its pose,
/// and watches for commands. Returns on Leave (to character select) or a dropped link.
fn run_in_world(
    session: &mut Session,
    cmd_rx: &Receiver<Cmd>,
    updates: &Sender<Update>,
    user: &str,
    self_guid: u64,
    start: [f32; 3],
    terrain: &mut Option<Terrain>,
    map_id: u32,
) -> InWorldExit {
    let _ = session.ready_stream();
    let mut pos = start;
    clamp_z(&mut pos, terrain);
    let mut orientation = 0.0_f32;
    let mut goal = Goal::Idle;
    let mut moving = false;
    let mut players: HashMap<u64, [f32; 3]> = HashMap::new();
    let mut last_tick = Instant::now();
    let mut last_pose = Instant::now();
    // Stuck detection for path walks: last position where we made progress, and when.
    let mut progress_ref = pos;
    let mut progress_at = Instant::now();
    // Shared navmesh for this map, for precise surface-height placement on floors/stairs.
    let nav = crate::navdt::shared(map_id);

    let send_pose = |pos: [f32; 3], moving: bool| {
        let _ = updates.send(Update::Pose {
            user: user.to_string(),
            pos,
            moving,
            map: map_id,
        });
    };
    let send_move = |status: MoveStatus| {
        let _ = updates.send(Update::Move { user: user.to_string(), status });
    };
    send_pose(pos, false);
    send_move(MoveStatus::default());

    loop {
        // Inbound: one packet (blocks up to the short read timeout), decoded into the tracker. A
        // quiet-stream timeout or a close both surface as Err; `poll` tells them apart (no packet
        // is pending after a timeout, so nothing is lost).
        match session.recv_events() {
            Ok(events) => {
                for ev in &events {
                    track(&mut players, self_guid, ev);
                }
            }
            Err(_) => {
                if session.poll().is_err() {
                    return InWorldExit::Dropped(Some("connection lost".to_string()));
                }
            }
        }

        // Movement tick on a fixed cadence, independent of packet rate.
        if last_tick.elapsed() >= TICK {
            let dt = last_tick.elapsed().as_secs_f32();
            last_tick = Instant::now();

            // The current target waypoint, its arrival radius, and path bookkeeping (copied out so
            // the immutable borrow of `goal` is released before we may mutate it below).
            let (target, stop, is_path, endpoint, move_id) = match &goal {
                Goal::Idle => (None, 0.0, false, None, 0u64),
                Goal::Point(p) => (Some(*p), ARRIVE_EPS, false, None, 0),
                Goal::FollowNearest => (nearest_player(&players, pos), FOLLOW_STOP, false, None, 0),
                Goal::Path { wps, endpoint, move_id } => {
                    let r = if wps.len() <= 1 { ARRIVE_EPS } else { WAYPOINT_EPS };
                    (wps.front().copied(), r, true, Some(*endpoint), *move_id)
                }
            };

            match target {
                Some(t) => {
                    let (dx, dy) = (t[0] - pos[0], t[1] - pos[1]);
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist <= stop {
                        // Reached this target/waypoint; reset the stuck timer.
                        progress_ref = pos;
                        progress_at = Instant::now();
                        // Pop the reached waypoint and decide the outcome without holding a borrow of
                        // `goal` across the reassignments below. `path_done` = the path's last
                        // waypoint was reached; `point_arrival` = a Point/Follow target was reached.
                        let mut path_done: Option<([f32; 3], u64)> = None;
                        let mut point_arrival = false;
                        match &mut goal {
                            Goal::Path { wps, endpoint, move_id } => {
                                wps.pop_front();
                                if wps.is_empty() {
                                    path_done = Some((*endpoint, *move_id));
                                }
                                // else: next tick pursues the next waypoint (keep running).
                            }
                            _ => point_arrival = true,
                        }
                        if let Some((ep, mid)) = path_done {
                            if moving {
                                // Don't re-clamp to the ground here: the moving tick already placed Z
                                // correctly (ground on open terrain, navmesh floor height on a
                                // structure), and a ground clamp would drop a building-floor arrival.
                                let _ = session.halt(pos, orientation);
                                moving = false;
                                send_pose(pos, false);
                            }
                            send_move(MoveStatus {
                                state: MoveState::Arrived,
                                goal: Some(ep),
                                remaining: 0.0,
                                move_id: mid,
                            });
                            goal = Goal::Idle;
                        } else if point_arrival {
                            if moving {
                                clamp_z(&mut pos, terrain);
                                let _ = session.halt(pos, orientation);
                                moving = false;
                                send_pose(pos, false);
                            }
                            if let Goal::Point(_) = goal {
                                goal = Goal::Idle; // a point is reached once; follow keeps trailing
                            }
                        }
                    } else {
                        orientation = dy.atan2(dx); // WoW orientation: 0 faces +X (north)
                        // Advance toward the waypoint's XY at run speed.
                        let frac = (RUN_SPEED * dt).min(dist) / dist;
                        let nx = pos[0] + dx * frac;
                        let ny = pos[1] + dy * frac;
                        // Body height at the new XY: prefer the EXACT ADT ground, but when the precise
                        // navmesh surface there sits off the terrain by more than FLOOR_EPS we're on a
                        // structure (floor/stairs) — stand on the navmesh surface so she isn't buried by
                        // the ground under a floor. (getPolyHeight is exact, so a small eps is safe and
                        // even a low ground-floor is caught; open terrain keeps the exact ground.)
                        let ground = terrain.as_mut().and_then(|tr| tr.height(nx, ny));
                        let surf = nav
                            .as_ref()
                            .and_then(|n| n.lock().ok().and_then(|q| q.height([nx, ny, pos[2]])));
                        let target_z = match (surf, ground) {
                            (Some(s), Some(g)) => if (s - g).abs() > FLOOR_EPS { s } else { g },
                            (Some(s), None) => s,
                            (None, Some(g)) => g,
                            (None, None) => pos[2] + (t[2] - pos[2]) * frac,
                        };
                        // Apply, capping the 3D step at run speed so climbs stay legal (vertical gain
                        // slows the horizontal advance instead of spiking total speed).
                        let (mut mx, mut my, mut mz) = (nx - pos[0], ny - pos[1], target_z - pos[2]);
                        let mag = (mx * mx + my * my + mz * mz).sqrt();
                        let budget = RUN_SPEED * dt;
                        if mag > budget {
                            let s = budget / mag;
                            mx *= s;
                            my *= s;
                            mz *= s;
                        }
                        pos[0] += mx;
                        pos[1] += my;
                        pos[2] += mz;
                        if moving {
                            let _ = session.moving(pos, orientation);
                        } else {
                            let _ = session.start_forward(pos, orientation);
                            moving = true;
                        }
                        // Stuck detection (path walks only).
                        if (pos[0] - progress_ref[0]).hypot(pos[1] - progress_ref[1]) > STUCK_EPS {
                            progress_ref = pos;
                            progress_at = Instant::now();
                        } else if is_path && progress_at.elapsed().as_secs_f32() > STUCK_SECS {
                            let _ = session.halt(pos, orientation);
                            moving = false;
                            send_pose(pos, false);
                            let ep = endpoint.unwrap_or(pos);
                            send_move(MoveStatus {
                                state: MoveState::Blocked,
                                goal: Some(ep),
                                remaining: (pos[0] - ep[0]).hypot(pos[1] - ep[1]),
                                move_id,
                            });
                            goal = Goal::Idle;
                        }
                        if last_pose.elapsed() >= Duration::from_millis(150) {
                            send_pose(pos, true);
                            if is_path {
                                let ep = endpoint.unwrap_or(pos);
                                send_move(MoveStatus {
                                    state: MoveState::Moving,
                                    goal: Some(ep),
                                    remaining: (pos[0] - ep[0]).hypot(pos[1] - ep[1]),
                                    move_id,
                                });
                            }
                            last_pose = Instant::now();
                        }
                    }
                }
                None => {
                    if moving {
                        let _ = session.halt(pos, orientation);
                        moving = false;
                        send_pose(pos, false);
                    }
                }
            }
        }

        match cmd_rx.try_recv() {
            Ok(Cmd::MoveTo(t)) => goal = Goal::Point(t),
            Ok(Cmd::Walk { waypoints, endpoint, move_id }) => {
                progress_ref = pos;
                progress_at = Instant::now();
                send_move(MoveStatus {
                    state: MoveState::Moving,
                    goal: Some(endpoint),
                    remaining: (pos[0] - endpoint[0]).hypot(pos[1] - endpoint[1]),
                    move_id,
                });
                goal = Goal::Path { wps: waypoints.into_iter().collect(), endpoint, move_id };
            }
            Ok(Cmd::Follow) => goal = Goal::FollowNearest,
            Ok(Cmd::Stop) => {
                goal = Goal::Idle;
                if moving {
                    let _ = session.halt(pos, orientation);
                    moving = false;
                    send_pose(pos, false);
                }
                send_move(MoveStatus::default());
            }
            Ok(Cmd::Leave) => {
                if moving {
                    let _ = session.halt(pos, orientation);
                }
                let _ = session.leave();
                return InWorldExit::CharSelect;
            }
            Ok(Cmd::RawMove(t)) => {
                // The adversarial primitive: claim EXACTLY t, no clamp. Idle so the movement tick
                // doesn't walk away or re-clamp. The server's anti-cheat adjudicates the claim.
                goal = Goal::Idle;
                pos = t;
                let _ = session.start_forward(pos, orientation);
                let _ = session.moving(pos, orientation);
                let _ = session.halt(pos, orientation);
                moving = false;
                send_pose(pos, false);
            }
            Ok(Cmd::Logout) => return InWorldExit::Dropped(None),
            Ok(_) => {} // Enter/Create ignored while already in the world
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return InWorldExit::Dropped(None),
        }
    }
}
