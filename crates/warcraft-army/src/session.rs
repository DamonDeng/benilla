//! Headless bot sessions on a 1.12.1 (vmangos) server, built on `benilla-protocol`'s client
//! stack — the same auth + world handshake the graphical client uses, with no renderer. One
//! [`Session`] is one authenticated connection for one account; the army manager holds several,
//! one per bot, and the server treats each as an ordinary player.
//!
//! This is the "body" layer: connect, read the roster, enter the world as a character, leave.
//! The agent's senses (parsing the world stream) and actions (movement, casting) build on top of
//! the live [`WorldSession`] later; for now this proves the plumbing end to end.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use benilla_protocol::messages::{
    Character, CharCreateReq, CHAR_CREATE_NAME_IN_USE, CHAR_CREATE_SERVER_LIMIT,
    CHAR_CREATE_SUCCESS,
};
use benilla_protocol::{auth, decode, logon, SessionEvent, WorldSession, WORLD_PORT};

/// How long [`Session::leave`] waits for `SMSG_LOGOUT_COMPLETE`. Logging out in the open field runs
/// the 20 s camp timer (instant only in cities/inns), so this must exceed it.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(25);

/// An account the army is entrusted to drive. The password lives here (and, in the app, in the
/// local `army-secrets.toml`), never in the server DB — SRP6 keeps only a verifier there, so a
/// plaintext password is required to log in and cannot be recovered from the database.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: String,
    pub pass: String,
    /// The realmd address, `host[:port]` (port defaults to 3724).
    pub realmlist: String,
}

/// A character on an account's roster, trimmed to what the UI shows. `race`/`class`/`gender` are
/// the raw `ChrRaces`/`ChrClasses` ids; the UI maps them to names.
#[derive(Clone, Debug)]
pub struct CharInfo {
    pub guid: u64,
    pub name: String,
    pub race: u8,
    pub class: u8,
    pub gender: u8,
    pub level: u8,
    /// `Map.dbc` id the character is on (0 Azeroth, 1 Kalimdor, …), for terrain lookup.
    pub map: u32,
    /// Saved position in WoW world coords `[x, y, z]` — where the character spawns on login, and
    /// the dead-reckoning start for movement (movement is client-authoritative).
    pub pos: [f32; 3],
}

/// A character to create for a bot. The appearance dials are `CharSections` indices; the UI bounds
/// them to the valid per-race/gender counts from `CharCreateCatalog`, so the server accepts them.
#[derive(Clone, Debug)]
pub struct NewChar {
    pub name: String,
    pub race: u8,
    pub class: u8,
    pub gender: u8,
    pub skin: u8,
    pub face: u8,
    pub hair_style: u8,
    pub hair_color: u8,
    pub facial_hair: u8,
}

impl From<&Character> for CharInfo {
    fn from(c: &Character) -> Self {
        Self {
            guid: c.guid,
            name: c.name.clone(),
            race: c.race,
            class: c.class,
            gender: c.gender,
            level: c.level,
            map: c.map,
            pos: [c.position.x, c.position.y, c.position.z],
        }
    }
}

/// One authenticated world connection. Dropping it closes the socket, which is the right way to
/// end a session that is only parked at character select; a session that entered the world should
/// [`Session::leave`] first so the server records a clean logout.
pub struct Session {
    pub user: String,
    inner: WorldSession,
    /// The account's characters, read at login.
    pub roster: Vec<CharInfo>,
    /// The character currently in the world, if any.
    pub in_world: Option<u64>,
}

impl Session {
    /// Authenticate against realmd, connect to the realm's world server, and read the roster.
    /// Leaves the session parked at character select (as the glue screens are).
    pub fn login(acct: &Account) -> Result<Self> {
        let l = logon(
            &acct.realmlist,
            &acct.user,
            &acct.pass,
            auth::LOCALE_TAG_EN_US,
        )
        .with_context(|| format!("realmd logon for {}", acct.user))?;

        // The realm's own world address, or the realmd host with the default world port.
        let addr = l
            .realms
            .first()
            .map(|r| r.address.clone())
            .unwrap_or_else(|| format!("{}:{WORLD_PORT}", host_of(&acct.realmlist)));

        let mut inner = WorldSession::connect(&addr, &acct.user, l.session_key)
            .with_context(|| format!("world connect for {}", acct.user))?;

        let roster = inner
            .char_enum()
            .with_context(|| format!("reading the roster for {}", acct.user))?
            .iter()
            .map(CharInfo::from)
            .collect();

        Ok(Self {
            user: acct.user.clone(),
            inner,
            roster,
            in_world: None,
        })
    }

    /// Enter the world as `guid` and become the mover the server simulates for us.
    pub fn enter(&mut self, guid: u64) -> Result<()> {
        self.inner
            .player_login(guid)
            .with_context(|| format!("{} entering world as {guid:#x}", self.user))?;
        self.inner.set_active_mover(guid)?;
        self.in_world = Some(guid);
        Ok(())
    }

    /// Log out of the world back to character select (only meaningful once [`Self::enter`] ran).
    pub fn leave(&mut self) -> Result<()> {
        self.inner.logout(LOGOUT_TIMEOUT)?;
        self.in_world = None;
        Ok(())
    }

    /// Set a short socket read timeout for the in-world loop, so `recv_events` paces the loop when
    /// the stream is quiet and returns promptly so movement and commands stay responsive.
    pub fn ready_stream(&mut self) -> Result<()> {
        self.inner.set_read_timeout(Some(Duration::from_millis(100)))
    }

    /// Read one inbound packet (blocking up to the stream read timeout) and decode it into session
    /// events. `Err` on a quiet-stream timeout *or* a closed link; the caller disambiguates a close
    /// with [`Self::poll`].
    pub fn recv_events(&mut self) -> Result<Vec<SessionEvent>> {
        Ok(decode(self.inner.recv()?))
    }

    /// A non-blocking liveness check: `Err` means the server closed the link. Used after a
    /// `recv_events` timeout to tell a quiet stream from a dead one.
    pub fn poll(&mut self) -> Result<()> {
        self.inner.poll_link()
    }

    /// Begin running forward toward `orientation` (`MSG_MOVE_START_FORWARD`).
    pub fn start_forward(&mut self, pos: [f32; 3], orientation: f32) -> Result<()> {
        self.inner.start_forward(pos, orientation)
    }

    /// Continue running (`MSG_MOVE_HEARTBEAT`), one per movement tick.
    pub fn moving(&mut self, pos: [f32; 3], orientation: f32) -> Result<()> {
        self.inner.heartbeat(pos, orientation)
    }

    /// Stop (`MSG_MOVE_STOP`), so observers see the run end.
    pub fn halt(&mut self, pos: [f32; 3], orientation: f32) -> Result<()> {
        self.inner.stop(pos, orientation)
    }

    /// Create a character on this account (at character select) and refresh the roster. Appearance
    /// is all-zero (the first valid `CharSections` slot for any race/gender).
    pub fn create(&mut self, spec: &NewChar) -> Result<()> {
        let req = CharCreateReq {
            name: spec.name.clone(),
            race: spec.race,
            class: spec.class,
            gender: spec.gender,
            skin: spec.skin,
            face: spec.face,
            hair_style: spec.hair_style,
            hair_color: spec.hair_color,
            facial_hair: spec.facial_hair,
        };
        match self.inner.create_character(&req)? {
            CHAR_CREATE_SUCCESS => {}
            CHAR_CREATE_NAME_IN_USE => bail!("the name \"{}\" is already in use", spec.name),
            CHAR_CREATE_SERVER_LIMIT => bail!("this account has reached its character limit"),
            other => bail!(
                "server refused the character (code {other:#04x}) — often an invalid race/class \
                 combination for 1.12 (e.g. Human cannot be a Shaman)"
            ),
        }
        self.roster = self
            .inner
            .char_enum()
            .context("re-reading the roster after create")?
            .iter()
            .map(CharInfo::from)
            .collect();
        Ok(())
    }
}

/// The host part of a `host[:port]` string (an IPv6 literal in brackets is returned whole).
fn host_of(realmlist: &str) -> &str {
    benilla_protocol::host_port(realmlist, WORLD_PORT).0
}

/// `ChrRaces.dbc` id → name, for display.
pub fn race_name(race: u8) -> &'static str {
    match race {
        1 => "Human",
        2 => "Orc",
        3 => "Dwarf",
        4 => "Night Elf",
        5 => "Undead",
        6 => "Tauren",
        7 => "Gnome",
        8 => "Troll",
        _ => "?",
    }
}

/// `ChrClasses.dbc` id → name, for display.
pub fn class_name(class: u8) -> &'static str {
    match class {
        1 => "Warrior",
        2 => "Paladin",
        3 => "Hunter",
        4 => "Rogue",
        5 => "Priest",
        7 => "Shaman",
        8 => "Mage",
        9 => "Warlock",
        11 => "Druid",
        _ => "?",
    }
}
