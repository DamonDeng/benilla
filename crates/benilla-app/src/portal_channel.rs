//! The portal transit channel: a deliberate, interruptible "pull" before a portal GameObject fires.
//!
//! Right-clicking a portal (a GOOBER whose display model is a portal swirl/stone) does not use it at
//! once. Instead the player is driven into a real channelled spell *locally* — the cast pose and the
//! spell's own beam aimed at the portal — by setting `UNIT_CHANNEL_SPELL`/`UNIT_CHANNEL_OBJECT`,
//! which benilla's `route_cast_visuals` turns into the channel-hold animation and `chain_beam` turns
//! into the beam (ending on the portal stone's body — see `TARGET_HEIGHT_FACTOR` in `chain_beam`).
//! It holds for a minimum time; only on completion does the client send `CMSG_GAMEOBJ_USE` (letting
//! the server teleport). Moving cancels it. The channel-field writes are purely local (never sent on
//! the wire; the server never set them, so it does not clobber them for the ~5 s). The visible half
//! of the online-content transit UX (`docs/online-content-updates.md`, Phase C).

use bevy::prelude::*;

use crate::net::{ClientCommand, GuidIndex, NetCommands, ObjectStore, SelfPlayer};

/// Minimum channel time before the portal fires (seconds).
const MIN_SECS: f32 = 5.0;
/// Moving more than this many yards from where the channel began cancels it.
const CANCEL_DIST: f32 = 1.5;
/// `GAMEOBJECT_TYPE_GOOBER` — the only GO type routed through the channel; non-portal goobers pass
/// straight through to a normal use.
pub(crate) const GO_TYPE_GOOBER: i32 = 10;
/// Portal swirl/stone display models (`GameObjectDisplayInfo`): Mystical Portal, Dark Portal, Portal.
const PORTAL_DISPLAY_IDS: &[i32] = &[2770, 559, 6696];
/// The spell channelled during the transit — it supplies both the cast pose and the beam. Mind Flay
/// by default (a shadow channel: a channelling stance + a purple beam to the target); overridable
/// with `BENILLA_PORTAL_SPELL`. Use a *channel* spell (the stance/beam come from its channel kit).
const DEFAULT_PORTAL_SPELL: u32 = 15407; // Mind Flay

/// A right-click asked to use a goober; the channel system decides portal (channel) vs. pass-through.
#[derive(Message, Clone, Copy)]
pub struct PortalChannelStart {
    pub go_guid: u64,
}

#[derive(Clone, Copy)]
struct Active {
    go_guid: u64,
    elapsed: f32,
    /// Player position when the channel began; moving away from it cancels.
    anchor: Vec3,
}

#[derive(Resource, Default)]
struct PortalChannel(Option<Active>);

/// The spell channelled during the transit; `BENILLA_PORTAL_SPELL` overrides the default.
fn portal_spell_id() -> u32 {
    std::env::var("BENILLA_PORTAL_SPELL")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DEFAULT_PORTAL_SPELL)
}

/// Begin/advance/finish the channel: drive a client-only channelled spell on the player (pose + beam
/// to the portal), and fire the portal on completion.
fn portal_channel(
    mut starts: MessageReader<PortalChannelStart>,
    mut chan: ResMut<PortalChannel>,
    index: Res<GuidIndex>,
    commands: Res<NetCommands>,
    time: Res<Time>,
    // `Without<SelfPlayer>` keeps this &ObjectStore from aliasing the player's &mut ObjectStore below.
    gos: Query<(&ObjectStore, &GlobalTransform), Without<SelfPlayer>>,
    mut me: Query<&mut ObjectStore, With<SelfPlayer>>,
    me_pos: Query<&GlobalTransform, With<SelfPlayer>>,
) {
    let Some(mut player_store) = me.iter_mut().next() else {
        chan.0 = None; // no player in world
        return;
    };
    let Some(player_pos) = me_pos.iter().next().map(|t| t.translation()) else {
        chan.0 = None;
        return;
    };

    // New requests: a portal-model goober starts the channel; anything else uses immediately.
    for s in starts.read() {
        let is_portal = index
            .0
            .get(&s.go_guid)
            .copied()
            .and_then(|e| gos.get(e).ok())
            .and_then(|(store, _)| store.0.gameobject_displayid())
            .is_some_and(|d| PORTAL_DISPLAY_IDS.contains(&d));
        if !is_portal {
            let _ = commands.0.send(ClientCommand::GameObjUse { guid: s.go_guid });
            continue;
        }
        let spell_id = portal_spell_id();
        // Benilla renders the channel pose + the spell's beam aimed at the portal from these fields.
        player_store.0.set_unit_channel(spell_id, s.go_guid);
        chan.0 = Some(Active {
            go_guid: s.go_guid,
            elapsed: 0.0,
            anchor: player_pos,
        });
        info!(
            "portal channel: channelling {spell_id} toward portal {:#x} (hold still ~{MIN_SECS:.0}s)",
            s.go_guid
        );
    }

    // Advance the active channel.
    let Some(mut active) = chan.0 else {
        return;
    };
    let portal_present = index
        .0
        .get(&active.go_guid)
        .copied()
        .is_some_and(|e| gos.get(e).is_ok());
    if !portal_present {
        player_store.0.clear_unit_channel();
        chan.0 = None;
        return;
    }
    if player_pos.distance(active.anchor) > CANCEL_DIST {
        info!("portal channel: cancelled — you moved");
        player_store.0.clear_unit_channel();
        chan.0 = None;
        return;
    }

    active.elapsed += time.delta_secs();
    if active.elapsed >= MIN_SECS {
        player_store.0.clear_unit_channel();
        let _ = commands.0.send(ClientCommand::GameObjUse { guid: active.go_guid });
        info!("portal channel: complete — entering portal {:#x}", active.go_guid);
        chan.0 = None;
    } else {
        chan.0 = Some(active);
    }
}

pub(crate) struct PortalChannelPlugin;

impl Plugin for PortalChannelPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PortalChannelStart>()
            .init_resource::<PortalChannel>()
            .add_systems(Update, portal_channel);
    }
}
