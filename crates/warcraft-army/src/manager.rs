//! The session manager: one background worker thread per account, so a blocking SRP6 login never
//! freezes the UI. The UI calls [`Manager::login`]/[`Manager::logout`]/[`Manager::create`] and
//! reads [`Manager::status`] + [`Manager::notices`], which [`Manager::pump`] refreshes each frame
//! from the workers' updates.
//!
//! Accounts are hardcoded here for now (the two test bots). The next increment swaps this for
//! vmangos-DB discovery plus an `army-secrets.toml` for the login passwords (the DB holds only
//! SRP6 verifiers, never plaintext, so passwords must come from the local file).

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use benilla_protocol::{EntityKind, SessionEvent};

use crate::session::{Account, CharInfo, NewChar, Session};
use crate::terrain::Terrain;

/// Movement-packet cadence and the min interval between movement ticks.
const TICK: Duration = Duration::from_millis(100);
/// Ground run speed (yd/s), the stock `MOVE_RUN`; the bot advances at most this per tick.
const RUN_SPEED: f32 = 7.0;
/// Horizontal distance (yd) at which the bot counts a move-to goal as reached.
const ARRIVE_EPS: f32 = 0.75;
/// How close the bot trails a followed player (yd) before it stops and waits.
const FOLLOW_STOP: f32 = 4.0;

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
    /// The in-world bot's current position and whether it's walking (for the live readout).
    Pose {
        user: String,
        pos: [f32; 3],
        moving: bool,
    },
}

/// A command from the UI to a running worker.
enum Cmd {
    Create(NewChar),
    /// Enter the world as this character guid.
    Enter(u64),
    /// Walk to this WoW-coords point (straight-line; Phase A has no obstacle avoidance).
    MoveTo([f32; 3]),
    /// Follow the nearest player, trailing a few yards behind.
    Follow,
    /// Stop walking / following where you are.
    Stop,
    /// Leave the world back to character select, staying logged in.
    Leave,
    Logout,
}

/// How an in-world loop ended.
enum InWorldExit {
    /// Back to character select, session still alive.
    CharSelect,
    /// The session is finished (clean logout, or a drop carrying its reason).
    Dropped(Option<String>),
}

/// The accounts the army may drive. Hardcoded for now; becomes DB discovery + `army-secrets.toml`.
pub fn default_accounts() -> Vec<Account> {
    ["TESTONE", "TESTTWO"]
        .iter()
        .map(|u| Account {
            user: (*u).to_string(),
            pass: "<password>".to_string(),
            realmlist: "127.0.0.1".to_string(),
        })
        .collect()
}

pub struct Manager {
    pub accounts: Vec<Account>,
    pub status: HashMap<String, Status>,
    /// Latest transient message per account (create refusals, etc.).
    pub notices: HashMap<String, String>,
    /// Latest in-world pose per account: `(position, moving)`.
    poses: HashMap<String, ([f32; 3], bool)>,
    updates_rx: Receiver<Update>,
    updates_tx: Sender<Update>,
    workers: HashMap<String, Sender<Cmd>>,
}

impl Manager {
    pub fn new(accounts: Vec<Account>) -> Self {
        let (updates_tx, updates_rx) = channel();
        let status = accounts
            .iter()
            .map(|a| (a.user.clone(), Status::Offline))
            .collect();
        Self {
            accounts,
            status,
            notices: HashMap::new(),
            poses: HashMap::new(),
            updates_rx,
            updates_tx,
            workers: HashMap::new(),
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
        while let Ok(update) = self.updates_rx.try_recv() {
            match update {
                Update::Status { user, status } => {
                    if matches!(status, Status::Offline | Status::Error(_)) {
                        self.workers.remove(&user);
                    }
                    self.status.insert(user, status);
                }
                Update::Notice { user, text } => {
                    self.notices.insert(user, text);
                }
                Update::Pose { user, pos, moving } => {
                    self.poses.insert(user, (pos, moving));
                }
            }
        }
        // Repaint while any worker is live: connecting accounts flip soon, and in-world ones can
        // change state (e.g. a dropped connection) without a UI input to trigger a frame.
        self.status
            .values()
            .any(|s| s.is_connecting() || s.is_online())
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
                match run_in_world(&mut session, &cmd_rx, &updates, &user, guid, start, &mut terrain)
                {
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
            Ok(Cmd::Leave) | Ok(Cmd::MoveTo(_)) | Ok(Cmd::Follow) | Ok(Cmd::Stop) => {}
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

    let send_pose = |pos: [f32; 3], moving: bool| {
        let _ = updates.send(Update::Pose {
            user: user.to_string(),
            pos,
            moving,
        });
    };
    send_pose(pos, false);

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

            let (target, stop) = match goal {
                Goal::Idle => (None, 0.0),
                Goal::Point(p) => (Some(p), ARRIVE_EPS),
                Goal::FollowNearest => (nearest_player(&players, pos), FOLLOW_STOP),
            };

            match target {
                Some(t) => {
                    let (dx, dy) = (t[0] - pos[0], t[1] - pos[1]);
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist <= stop {
                        if moving {
                            clamp_z(&mut pos, terrain);
                            let _ = session.halt(pos, orientation);
                            moving = false;
                            send_pose(pos, false);
                        }
                        if let Goal::Point(_) = goal {
                            goal = Goal::Idle; // a point is reached once; follow keeps trailing
                        }
                    } else {
                        orientation = dy.atan2(dx); // WoW orientation: 0 faces +X (north)
                        let frac = (RUN_SPEED * dt).min(dist) / dist;
                        pos[0] += dx * frac;
                        pos[1] += dy * frac;
                        match terrain.as_mut().and_then(|tr| tr.height(pos[0], pos[1])) {
                            Some(gz) => pos[2] = gz,
                            None => pos[2] += (t[2] - pos[2]) * frac,
                        }
                        if moving {
                            let _ = session.moving(pos, orientation);
                        } else {
                            let _ = session.start_forward(pos, orientation);
                            moving = true;
                        }
                        if last_pose.elapsed() >= Duration::from_millis(150) {
                            send_pose(pos, true);
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
            Ok(Cmd::Follow) => goal = Goal::FollowNearest,
            Ok(Cmd::Stop) => {
                goal = Goal::Idle;
                if moving {
                    let _ = session.halt(pos, orientation);
                    moving = false;
                    send_pose(pos, false);
                }
            }
            Ok(Cmd::Leave) => {
                if moving {
                    let _ = session.halt(pos, orientation);
                }
                let _ = session.leave();
                return InWorldExit::CharSelect;
            }
            Ok(Cmd::Logout) => return InWorldExit::Dropped(None),
            Ok(_) => {} // Enter/Create ignored while already in the world
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return InWorldExit::Dropped(None),
        }
    }
}
