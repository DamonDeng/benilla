//! Explore mode: the client booted server-less into the open world on the full player controller
//! ([`crate::player`]) — no login, no server, no enemies. Started by [`crate::run_explore`] (the
//! `warcraft-explorer` launcher) or `WOW_EXPLORE=1`; `WOW_EXPLORE_AT="map,x,y,z[,o]"` (WoW
//! coordinates) picks the spawn, Northshire Valley by default.
//!
//! The mode spawns one local unit tagged [`SelfPlayer`], and everything else is the ordinary
//! client: `player::embody` attaches the controller to the body, and the take-control edge
//! (`player::wire_in`) seizes it from the body's own transform — the part a server normally plays
//! is only that the body exists. No [`crate::run_mode::CaptureMode`] is inserted, so the
//! controller runs and the FrameXML interface loads.
//!
//! Two server-less seams any such mode must fill, or the stock UI misbehaves:
//! - The self store carries the player descriptor fields a create block would: without a valid
//!   rest byte, `GetRestState()` answers nils and `MainMenuBar.lua`'s exhaustion tick errors.
//! - A one-character roster is seeded ([`seed_identity`]): the per-character stores (the chat
//!   cache, macros, saved variables, the saved camera) resolve their identity through
//!   [`crate::ui_macro::identity`], and with no picked roster row the chat-cache restore skips,
//!   `UPDATE_CHAT_WINDOWS` never reaches FrameXML, and every chat window renders unstyled.

use bevy::prelude::*;

use benilla_assets::coords::wow_to_bevy;
use benilla_protocol::ObjectFields;

use crate::creature_anim::MovementState;
use crate::glue_session::roster::Roster;
use crate::net::{Guid, GuidIndex, NetEntity, ObjectStore, SelfGuid, SelfPlayer};

/// Set by [`crate::run_explore`] before the app builds; `WOW_EXPLORE=1` turns it on from outside.
pub(crate) static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn active() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var_os("WOW_EXPLORE").is_some_and(|v| !v.is_empty())
}

/// Where the run starts, in WoW coordinates; `orientation` is the WoW facing, which is also the
/// Bevy yaw (both measure the same rotation).
struct Spawn {
    map: u32,
    pos: [f32; 3],
    orientation: f32,
}

/// Northshire Valley: the human start (vmangos `playercreateinfo`), facing the abbey.
const NORTHSHIRE: Spawn = Spawn {
    map: 0,
    pos: [-8949.95, -132.493, 83.5312],
    orientation: 5.275,
};

/// `WOW_EXPLORE_AT="map,x,y,z[,o]"`, else [`NORTHSHIRE`]. A value that does not parse panics
/// rather than walking somewhere else, as `benilla_world::world_map`'s `$WOW_MAP` does.
fn spawn_point() -> Spawn {
    let Ok(text) = std::env::var("WOW_EXPLORE_AT") else {
        return NORTHSHIRE;
    };
    let v: Vec<f32> = text
        .split(',')
        .map(|t| {
            t.trim()
                .parse()
                .unwrap_or_else(|_| panic!("WOW_EXPLORE_AT: `{t}` is not a number in `{text}`"))
        })
        .collect();
    match v[..] {
        [map, x, y, z] => Spawn {
            map: map as u32,
            pos: [x, y, z],
            orientation: 0.0,
        },
        [map, x, y, z, o] => Spawn {
            map: map as u32,
            pos: [x, y, z],
            orientation: o,
        },
        _ => panic!("WOW_EXPLORE_AT wants `map,x,y,z[,o]`, got `{text}`"),
    }
}

/// The `Map.dbc` id the run starts on, which the launcher seats as `$WOW_MAP` before plugins
/// build ([`benilla_world::world_map`] reads that at `Startup`).
pub(crate) fn start_map() -> u32 {
    spawn_point().map
}

/// The explorer's serial under the creature guid high part; the roster identity shares the guid.
const EXPLORER_SERIAL: u64 = 1;

/// The creature high part (`HIGHGUID_UNIT`), as the net layer's streamed creatures carry.
const HIGH_CREATURE: u64 = 0xF130 << 48;

/// `FactionTemplate.dbc` 35: friendly to players.
const FACTION_FRIENDLY: u32 = 35;

/// Warrior-trainer `CreatureDisplayInfo` ids, one per race and sex, resolved against the creature
/// catalog at spawn; human male preferred.
const WARRIOR_LOOKS: &[u32] = &[
    1300, 1374, 1375, 1504, 1578, 1599, 1707, 1721, 1880, 2096, 2103, 2113, 2196, 2198, 2614, 2620,
    2658, 3053, 3054, 3055, 3280, 3287, 3343, 3399, 3431, 3743, 3793, 3794, 4242, 4556, 6071,
    11037,
];

/// Spawns the explorer's body once the creature data can dress it; the rest is the stock client.
pub(crate) struct ExplorePlugin;

impl Plugin for ExplorePlugin {
    fn build(&self, app: &mut App) {
        // The local identity, inserted at build time (after `GamePlugins` seats the default
        // `Roster`), so it is present before the world-entry UI load reads it: without a picked
        // roster row the chat-cache restore is skipped, the bindings and chat windows never load,
        // and movement input is dead.
        app.insert_resource(explorer_roster());
        app.add_systems(Update, (spawn_explorer, settle_account_data));
    }
}

/// Stand in for the server's account-data digests. A world entry leaves every section pending for
/// `SMSG_ACCOUNT_DATA_TIMES`; off-line it never arrives, so the chat cache, bindings, macros and
/// layout readers would wait forever (the chat windows render unstyled and keys stay unbound).
/// Idempotent and self-healing: it settles each section against its local file once, and the
/// deferred entry load's readers then run (held ones owed, later ones inline).
fn settle_account_data(data: Option<Res<crate::account_data::AccountData>>, roster: Res<Roster>) {
    if let Some(data) = data {
        data.settle_offline(crate::ui_macro::identity(&roster));
    }
}

/// The local identity: one picked roster row, so the per-character stores (chat cache, macros,
/// saved variables, saved camera, the bindings) have a realm and a name with no login.
fn explorer_roster() -> Roster {
    let mut roster = Roster::default();
    roster.chars = vec![benilla_protocol::Character {
        guid: HIGH_CREATURE | EXPLORER_SERIAL,
        name: "Explorer".into(),
        race: 1,
        class: 1,
        gender: 0,
        skin: 0,
        face: 0,
        hair_style: 0,
        hair_color: 0,
        facial_hair: 0,
        level: 60,
        new_character: false,
        zone: 0,
        map: 0,
        position: benilla_protocol::wire::Vector3d {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        flags: 0,
        equipment: [benilla_protocol::CharEnumItem::default(); 19],
        pet_display_id: 0,
        pet_level: 0,
        pet_family: 0,
    }];
    roster.pending_pick = Some(HIGH_CREATURE | EXPLORER_SERIAL);
    roster
}

/// The explorer: a warrior-trainer look (human male when the data has one), tagged [`SelfPlayer`]
/// so the embodiment and the take-control edge treat it as our streamed character. Registered in
/// [`GuidIndex`] and [`SelfGuid`] so by-guid readers (the unit feeds, the nameplates) resolve it.
fn spawn_explorer(
    mut commands: Commands,
    creatures: Option<Res<crate::entities::Creatures>>,
    mut index: ResMut<GuidIndex>,
    mut self_guid: ResMut<SelfGuid>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(creatures) = creatures.as_deref() else {
        return;
    };
    // `(display, race, sex)` per resolvable look; empty until the display catalog loads.
    let looks: Vec<(u32, u8, u8)> = WARRIOR_LOOKS
        .iter()
        .filter_map(|&d| {
            creatures
                .display_race_sex(d)
                .map(|(race, sex)| (d, race, sex))
        })
        .collect();
    let Some(&(display_id, race, sex)) = looks
        .iter()
        .find(|&&(_, race, sex)| race == 1 && sex == 0)
        .or(looks.first())
    else {
        return;
    };
    *done = true;

    let at = spawn_point();
    let guid = HIGH_CREATURE | EXPLORER_SERIAL;
    // `UNIT_FIELD_BYTES_0`: race, class (1 warrior), gender, power type (1 rage).
    let bytes0 = u32::from(race) | 1 << 8 | u32::from(sex) << 16 | 1 << 24;
    // The unit block a create would carry, plus the player fields the serverless UI needs
    // (the module doc's first seam). Indices are the 1.12 descriptor layout.
    let pairs: &[(u16, u32)] = &[
        (0, guid as u32),
        (1, (guid >> 32) as u32),
        // `OBJECT_FIELD_TYPE`: object | unit.
        (2, 0x9),
        // `OBJECT_FIELD_SCALE_X`.
        (4, 1.0f32.to_bits()),
        // `UNIT_FIELD_HEALTH` / `_MAXHEALTH`, `_LEVEL`, `_FACTIONTEMPLATE`, `_BYTES_0`.
        (22, 100),
        (28, 100),
        (34, 60),
        (35, FACTION_FRIENDLY),
        (36, bytes0),
        // `PLAYER_BYTES_2` byte 3: rest state 2, normal (not rested).
        (194, 2 << 24),
        // `PLAYER_XP` and `PLAYER_NEXT_LEVEL_XP`: an empty bar under the level-60 ceiling.
        (716, 0),
        (717, 209_800),
        // `PLAYER_REST_STATE_EXPERIENCE`: no rested pool.
        (1175, 0),
    ];
    let e = commands
        .spawn((
            Guid(guid),
            NetEntity {
                kind: benilla_protocol::EntityKind::Unit,
                display_id: Some(display_id),
                scale: 1.0,
            },
            ObjectStore(ObjectFields::from_pairs(pairs)),
            MovementState::default(),
            Transform::from_translation(wow_to_bevy(at.pos))
                .with_rotation(Quat::from_rotation_y(at.orientation)),
            Visibility::default(),
            SelfPlayer,
        ))
        .id();
    index.0.insert(guid, e);
    self_guid.0 = Some(guid);
    info!(
        "explore: spawned the explorer (display {display_id}) at {:?} on map {}",
        at.pos, at.map
    );
}
