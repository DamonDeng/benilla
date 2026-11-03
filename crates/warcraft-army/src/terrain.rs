//! Headless ground-height lookup for clamping bot movement to the terrain. Movement is
//! client-authoritative, so the bot must report a ground-correct Z or observers see it float
//! (above a valley) or sink (into a hill). This loads ADT tiles from the player's game data (the
//! `WoW` link) on demand and caches them per tile, then interpolates the ground height under a
//! world point — the same heightmap the real client walks on.

use std::collections::HashMap;

use benilla_formats::{
    load_map_catalog, load_tile_mesh, open_chain, terrain_height_at, world_to_tile, Chain, ChunkMesh,
};

pub struct Terrain {
    chain: Chain,
    /// The map's MPQ art directory (e.g. "Azeroth", "Kalimdor"), from `Map.dbc`.
    map: String,
    /// Per-tile cache: `(tx, ty)` → its chunks, or `None` for a tile with no ADT (ocean/edge),
    /// so a missing tile is looked up once, not every tick.
    tiles: HashMap<(u32, u32), Option<Vec<ChunkMesh>>>,
}

impl Terrain {
    /// Open terrain for `map_id`, or `None` if the game data or the map's directory isn't found
    /// (movement then falls back to linear-Z interpolation).
    pub fn open(map_id: u32) -> Option<Terrain> {
        let dir = benilla_formats::wow_data()?;
        let mut chain = open_chain(&dir).ok()?;
        let catalog = load_map_catalog(&mut chain).ok()?;
        let map = catalog.directory(map_id)?.to_string();
        Some(Terrain {
            chain,
            map,
            tiles: HashMap::new(),
        })
    }

    /// Ground height at WoW world `(x, y)`, loading and caching the covering ADT tile. `None` where
    /// there is no tile or no triangle under the point (ocean, a hole, off-map).
    pub fn height(&mut self, x: f32, y: f32) -> Option<f32> {
        let key = world_to_tile(x, y);
        if !self.tiles.contains_key(&key) {
            let chunks = load_tile_mesh(&mut self.chain, &self.map, key.0, key.1)
                .ok()
                .map(|t| t.chunks);
            self.tiles.insert(key, chunks);
        }
        let chunks = self.tiles.get(&key)?.as_ref()?;
        terrain_height_at(chunks, [x, y, 0.0])
    }
}
