//! World-map state: the `Map.dbc` catalog and the current map id, for every subsystem that keys
//! off the map (the terrain and WDL streamers, the loading screen, time-of-day lighting).

use benilla_assets::MapCatalogRes;
use benilla_formats::load_map_catalog;
use bevy::prelude::*;

use benilla_assets::LockRecover;
use benilla_assets::{AssetSet, WorldAssets};

/// The map before the server names one: Eastern Kingdoms (`Map.dbc` id 0).
pub(crate) const DEFAULT_MAP_ID: u32 = 0;

/// `$WOW_MAP`: the `Map.dbc` id a server-less run starts on (`0` Azeroth, `1` Kalimdor), unset
/// giving [`DEFAULT_MAP_ID`]. A value that does not parse panics rather than falling back.
fn map_id_from_env() -> u32 {
    match std::env::var("WOW_MAP") {
        Err(_) => DEFAULT_MAP_ID,
        Ok(v) => v.trim().parse().unwrap_or_else(|_| {
            panic!("WOW_MAP={v:?} is not a Map.dbc id — it takes a NUMBER (0 = Azeroth, 1 = Kalimdor), not a name")
        }),
    }
}

/// The map the player is on, rewritten by the game on every worldport, login's included.
#[derive(Resource, Clone, Copy)]
pub struct CurrentMap(pub u32);

/// A cross-map transition, the world-scope teardown signal: every map-scoped strong-handle cache
/// clears on it, since one that kept its handles would pin every map visited. A clear is safe
/// mid-session, as live users hold their own handles.
#[derive(Message, Clone, Copy)]
pub struct MapChange;

/// Request a content refresh: re-read `Map.dbc` and rescan the loose index so a map added to the
/// overlay since startup becomes usable **without a client restart**. Emitted by the terrain
/// streamer when it meets a map id the catalog does not know (`map_id` is that map, for logging and,
/// later, a targeted fetch). The foundation of online content updates — see
/// `docs/online-content-updates.md`.
#[derive(Message, Clone, Copy)]
pub struct RefreshContent {
    pub map_id: u32,
}

/// The startup set the map catalog loads in, which the streamers' setup runs after.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WorldMapLoad;

/// Loads `Map.dbc` and seeds [`CurrentMap`] at startup, after the patch chain opens.
pub(crate) struct WorldMapPlugin;

impl Plugin for WorldMapPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MapChange>()
            .add_message::<RefreshContent>()
            .insert_resource(crate::content::ContentOrigin::from_env())
            .init_resource::<crate::content::FetchedMaps>()
            .add_systems(
                Startup,
                load_world_map.after(AssetSet::Open).in_set(WorldMapLoad),
            );
        app.add_systems(Update, (announce_map_change, refresh_content));
    }
}

/// Announce every [`CurrentMap`] flip as a [`MapChange`]; the startup seed and a same-map
/// worldport announce nothing.
pub(crate) fn announce_map_change(
    map: Option<Res<CurrentMap>>,
    mut last: Local<Option<u32>>,
    mut changes: MessageWriter<MapChange>,
) {
    let Some(map) = map else { return };
    if let Some(prev) = last.replace(map.0) {
        if prev != map.0 {
            info!(
                "map change: {prev} → {} — evicting map-scoped caches",
                map.0
            );
            changes.write(MapChange);
        }
    }
}

/// Read `Map.dbc` off the patch chain into [`MapCatalogRes`] and seed [`CurrentMap`].
fn load_world_map(mut commands: Commands, world_assets: Option<Res<WorldAssets>>) {
    let Some(world_assets) = world_assets else {
        return;
    };
    let mut chain = world_assets.chain.lock_recover();
    match load_map_catalog(&mut chain) {
        Ok(c) => {
            info!("Map.dbc: {} maps catalogued", c.len());
            commands.insert_resource(MapCatalogRes(c));
            // `$WOW_MAP` picks a server-less run's map (a named capture scenario sets it before
            // `Startup`); a live session's `player::wire_in` overwrites it from the server.
            let map = map_id_from_env();
            commands.insert_resource(CurrentMap(map));
        }
        Err(e) => error!("failed to load Map.dbc, cross-map teleport disabled: {e:#}"),
    }
}

/// On a [`RefreshContent`] request, rescan the loose index and rebuild [`MapCatalogRes`] from a
/// re-read `Map.dbc` — so a map added to the overlay since startup is catalogued and streamable
/// without a client restart. `CurrentMap` is untouched (the server owns it). The runtime half of
/// Phase A in `docs/online-content-updates.md`.
fn refresh_content(
    mut commands: Commands,
    world_assets: Option<Res<WorldAssets>>,
    mut requests: MessageReader<RefreshContent>,
    origin: Res<crate::content::ContentOrigin>,
    mut fetched: ResMut<crate::content::FetchedMaps>,
) {
    let ids: Vec<u32> = requests.read().map(|r| r.map_id).collect();
    if ids.is_empty() {
        return;
    }
    let Some(world_assets) = world_assets else {
        return;
    };

    // Phase B — fetch-on-miss: pull any still-unknown requested map from the content origin into
    // the overlay before rebuilding. With no origin set, this is a no-op and we behave as Phase A
    // (re-read whatever is already local).
    if let (Some(origin_dir), Some(overlay)) = (origin.0.as_ref(), benilla_formats::wow_data()) {
        for id in ids.iter().copied() {
            if fetched.0.contains_key(&id) {
                continue;
            }
            match crate::content::fetch_map(origin_dir, id, &overlay) {
                Ok(row) => {
                    fetched.0.insert(id, row);
                }
                Err(e) => warn!("content fetch: map {id} not available from origin — {e}"),
            }
        }
    }

    // Empty the loose index so the next asset open rescans the overlay (picks up newly-written
    // files); `LooseFiles::find` re-`get()`s the cell per lookup, so terrain/DBC loads see it.
    benilla_formats::install_index().clear();
    let mut chain = world_assets.chain.lock_recover();
    match load_map_catalog(&mut chain) {
        Ok(mut c) => {
            // Merge fetched custom-map rows on top of the shipped `Map.dbc` (no binary patching).
            for row in fetched.0.values() {
                c.upsert(
                    row.map_id,
                    &row.folder,
                    &row.map_name,
                    row.instance_type,
                    row.loading_screen,
                );
            }
            info!(
                "content refresh: Map.dbc re-read, {} maps catalogued ({} fetched), loose index rescanned",
                c.len(),
                fetched.0.len()
            );
            commands.insert_resource(MapCatalogRes(c));
        }
        Err(e) => error!("content refresh: failed to reload Map.dbc: {e:#}"),
    }
}
