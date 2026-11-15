//! Generate a brand-new, walkable benilla map from existing art — no new 3D models.
//!
//! The one tile is a BASIN: a flat floor the player spawns on, ringed by a steep hill whose crest
//! sits just inside the true tile edge (so the cut border hides on the hilltop) with the outer
//! chunk ring flagged impassable — the player is contained and never sees the tile's edge.
//!
//! Emits three loose files that layer over the install's MPQs:
//!   - `DBFilesClient/Map.dbc`                     (the real Map.dbc + one appended row)
//!   - `World/Maps/<Folder>/<Folder>.wdt`          (one tile present)
//!   - `World/Maps/<Folder>/<Folder>_<tx>_<ty>.adt` (16×16 basin MCNK chunks, one ground texture)
//!
//! It round-trips every file through benilla's own parsers before writing (the gate), then stages
//! an overlay `Data/` (real MPQs symlinked in + these loose files) so explore mode can load it
//! without touching the real install.
//!
//! Usage: warcraft-mapgen <real-Data-dir> <overlay-Data-dir>

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use benilla_dbc::{DbcParser, FieldType, Schema, SchemaField, StringRef, Value};

// --- map parameters (the experiment's defaults) ---
const MAP_ID: u32 = 800;
const FOLDER: &str = "Sandbox";
const TILE_X: u32 = 32;
const TILE_Y: u32 = 32;

// --- basin shape (all tunable) ---
// The one tile is a natural bowl: a gently undulating floor the player spawns on, ringed by steep
// hills whose crest line undulates along the perimeter (peaks and saddles) and sits just inside the
// true tile edge, so the cut border is hidden up on a flat hilltop and the player is contained. The
// shape is an edge-distance basin BIAS (yards to the nearest of the 4 edges) plus seeded noise.
const FLOOR_Z: f32 = 100.0; // mean interior height (spawn floor)
const FLOOR_NOISE_AMP: f32 = 2.5; // gentle floor undulation, +-yards (stays comfortably walkable)
const FLOOR_PERIOD: f32 = 85.0; // floor noise feature size, yards
const CREST_CENTER: f32 = 160.0; // mean crest height
const CREST_AMP: f32 = 25.0; // crest undulation -> raw crest ~135..185 along the rim
const CREST_MIN: f32 = 135.0; // floor-of-crest: clamp so no saddle dips below this (containment)
const CREST_INSET: f32 = 28.0; // crest/plateau starts this many yards inside the edge; flat to edge
const CORNER_RADIUS: f32 = 130.0; // rounded-rect corner radius (0 = sharp rectangle, ~266 = circle)
const BAND: f32 = 110.0; // mean distance-from-edge where the floor ends and the hill ramp begins
const BAND_VAR: f32 = 12.0; // ramp-start wobble along the rim -> differing hill footprints
const RAMP_BUMP_AMP: f32 = 3.0; // fine bumps on the hillside, faded to 0 at floor and crest seams
const HILL_PERIOD: f32 = 35.0; // hillside bump feature size, yards
const RIDGE_PERIOD: f32 = 30.0; // crest ridgeline feature size (~10 undulations around the rim)
const BAND_RIM_PERIOD: f32 = 60.0; // ramp-start variation feature size around the rim
const RIM_CIRCLE_R: f32 = 50.0; // radius the rim parameter is wrapped onto for seamless ridges
const SEED: u32 = 0x5EED_1337; // re-roll the whole terrain by changing this
const IMPASSABLE_ED: f32 = 60.0; // flag a chunk MCNK_IMPASSABLE if its nearest edge_dist is below this

// --- lake: a bowl dug into the floor and filled with still water (MCLQ) ---
const LAKE_X: f32 = -270.0; // world X of the bowl centre (open area south of the spawn)
const LAKE_Y: f32 = -380.0; // world Y of the bowl centre
const LAKE_RADIUS: f32 = 45.0; // bowl radius in yards; terrain dips within this, untouched outside
const LAKE_DEPTH: f32 = 12.0; // max bed drop below the floor, at the centre
const LAKE_FILL_Z: f32 = 96.0; // still-water surface Z (below the floor's ~97.5 min, so water sits only in the bowl)
const RING_INNER_MIN: f32 = 105.0; // containment gate: min terrain height on the ring's inner face
const NORMAL_EPS: f32 = 2.0; // central-difference step (yards) for per-vertex MCNR normals

// --- grid constants (match benilla-formats::terrain) ---
const TILE_SIZE: f32 = 533.333_3;
const CHUNK_SIZE: f32 = TILE_SIZE / 16.0;
const UNIT_SIZE: f32 = CHUNK_SIZE / 8.0;
const MAP_OFFSET: f32 = 32.0 * TILE_SIZE;

/// Static scenery baked into the ADT as M2 doodads — REUSED art only, no new models. Each tuple is
/// (model path, world X, world Y, uniform scale); the doodad sits on the terrain (its Z is
/// `surface_z` at that world x,y). The basin centre is world (-266.667, -266.667). Add rows here to
/// drop more scenery — this is pure map content, no engine or server change.
const DOODADS: &[(&str, f32, f32, f32)] = &[
    (
        "World\\Azeroth\\Elwynn\\PassiveDoodads\\Trees\\ElwynnTreeCanopy01.m2",
        -300.0,
        -280.0,
        1.0,
    ),
];

/// Static buildings baked into the ADT as WMOs — REUSED art only, no new models. Each tuple is
/// (WMO root path, world X, world Y, yaw degrees, sink yards). Same "pure map content" story as
/// DOODADS, but a WMO brings walls, interior and collision (benilla builds a collider from its
/// geometry), unlike a walk-through M2.
///
/// `sink` buries the model this far below the terrain. Buildings are authored with a tall base/plinth
/// meant to be sunk into sloped ground (the floor sits at grade somewhere, the base fills the gap
/// under the rest). On our FLAT basin nothing buries the base, so we lower the origin by the base
/// height to bring the walk-in floor to ground level — otherwise you meet a ~base-height step and
/// can't enter. This is the per-building analogue of WoW's hand-authored MODF `position.z`.
const WMOS: &[(&str, f32, f32, f32, f32)] = &[
    // (WMO root, world X, world Y, yaw°, sink yds). Sinks DERIVED from original map data via
    // `benilla-extract <Data> wmosink Azeroth -8947 -153 16 buildings\` = median of
    // (terrain − origin.z) over every real placement. Reliable for models WoW placed on open ADT
    // terrain (farms, barn, stable, blacksmith); the city/platform ones (inn, chapel, guild house)
    // have no ADT terrain under them so the tool can't measure them → left 0 pending a visual check.
    // NOTE: layout changed from two horizontal rows to two vertical COLUMNS (X -180 / X -360) so a
    // relaunch visibly proves the new ADT loaded; sinks are unchanged (data-derived, see above).
    ("World\\wmo\\Azeroth\\Buildings\\human_farm\\farm.wmo", -270.0, -150.0, 0.0, 3.3), // n=19 med 3.11 (3.3 looks right)
    ("World\\wmo\\Azeroth\\Buildings\\GoldshireInn\\GoldshireInn.wmo", -180.0, -140.0, 0.0, 0.0), // no ADT placements (city)
    ("World\\wmo\\Azeroth\\Buildings\\Chapel\\RedridgeChapel.wmo", -180.0, -200.0, 0.0, 0.0), // Chapel.wmo is a graybox (1 flat-gray texture); RedridgeChapel is fully textured (12)
    ("World\\wmo\\Azeroth\\Buildings\\GoldshireBlacksmith\\goldshireblacksmith.wmo", -180.0, -260.0, 0.0, 0.0), // n=3 med 0.00
    ("World\\wmo\\Azeroth\\Buildings\\HumanTwoStory\\HumanTwoStory.wmo", -180.0, -320.0, 0.0, -1.5), // n=4 med -1.50 (origin above floor)
    ("World\\wmo\\Azeroth\\Buildings\\Human_Barracks\\Human_Barracks.wmo", -180.0, -380.0, 0.0, -3.5), // n=2 med -3.50
    ("World\\wmo\\Azeroth\\Buildings\\GuardTower\\GuardTower.wmo", -360.0, -140.0, 0.0, 0.0), // n=5 noisy (hills), default 0
    ("World\\wmo\\Azeroth\\Buildings\\Human_Barn_Silo\\barn.wmo", -360.0, -200.0, 0.0, 1.4), // n=4 med 1.44
    ("World\\wmo\\Azeroth\\Buildings\\Duskwood_Stable\\Duskwood_Stable.wmo", -360.0, -260.0, 0.0, 0.0), // n=8 med -0.06
    ("World\\wmo\\Azeroth\\Buildings\\GuildHouses\\GuildHouseA.wmo", -360.0, -320.0, 0.0, 0.0), // no ADT placements (city)
    ("World\\wmo\\Azeroth\\Buildings\\Westfall_human_farm\\Westfall_human_farm.wmo", -360.0, -380.0, 0.0, 3.0), // n=4 med 3.00
];

/// Candidate ground textures, in preference order; the first present in the install is used.
const TEXTURE_CANDIDATES: &[&str] = &[
    "Tileset\\Elwynn\\ElwynnGrassBase.blp", // the real Elwynn grass (Northshire's own ADT uses this)
    "Tileset\\Barrens\\BarrensBaseDirt.blp",
    "Tileset\\Generic\\black.blp",
];

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_f32(buf: &mut [u8], off: usize, v: f32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// One IFF chunk: 4-byte (reversed, on-disk) magic + u32 LE size + payload.
fn iff(magic: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(magic);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// ADT tile → world (x, y) of its max corner (inverse of world_to_tile).
fn tile_to_world(tile_x: u32, tile_y: u32) -> (f32, f32) {
    (
        MAP_OFFSET - tile_y as f32 * TILE_SIZE,
        MAP_OFFSET - tile_x as f32 * TILE_SIZE,
    )
}

/// Inward distance (yards) from world (vx,vy) to a ROUNDED-rectangle boundary: straight hill-walls
/// along the four sides, quarter-circle arcs at the corners. Positive inside, 0 on the boundary.
/// The height profile reads this unchanged, so floor/ramp/crest/impassable all follow the rounding.
fn edge_dist(vx: f32, vy: f32, ox: f32, oy: f32) -> f32 {
    let c = TILE_SIZE / 2.0; // tile center, ~266.667
    let (lx, ly) = (ox - vx, oy - vy); // tile-local, 0..TILE from the max corner
    let px = (lx - c).abs(); // fold into one quadrant by symmetry
    let py = (ly - c).abs();
    let (hx, hy) = (c, c); // tile half-extents
    let r = CORNER_RADIUS;
    let dx = px - (hx - r);
    let dy = py - (hy - r);
    let outside = dx.max(0.0).hypot(dy.max(0.0));
    let inside = dx.max(dy).min(0.0);
    let sdf = outside + inside - r; // < 0 inside, 0 on boundary (rounded-rect signed distance)
    -sdf // positive = distance inward from the rounded edge
}

/// Smoothstep.
fn fade(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Integer finalizer (a good u32 avalanche hash) — the source of all pseudo-randomness here.
fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Lattice value at integer grid point (ix,iy), deterministic in (SEED, seed), in [-1,1].
fn lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    let h = hash_u32(
        (ix as u32)
            .wrapping_mul(0x9E37_79B1)
            ^ (iy as u32).wrapping_mul(0x85EB_CA77)
            ^ seed
            ^ SEED,
    );
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Bilinear value noise with a smoothstep fade, in [-1,1].
fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (ix, iy) = (x0 as i32, y0 as i32);
    let (fx, fy) = (fade(x - x0), fade(y - y0));
    let n00 = lattice(ix, iy, seed);
    let n10 = lattice(ix + 1, iy, seed);
    let n01 = lattice(ix, iy + 1, seed);
    let n11 = lattice(ix + 1, iy + 1, seed);
    let nx0 = n00 + (n10 - n00) * fx;
    let nx1 = n01 + (n11 - n01) * fx;
    nx0 + (nx1 - nx0) * fy
}

/// Fractal (multi-octave) value noise at world (x,y), period in yards, normalized to [-1,1].
fn fbm(x: f32, y: f32, period: f32, seed: u32, octaves: u32) -> f32 {
    let mut freq = 1.0 / period;
    let (mut amp, mut sum, mut norm) = (1.0f32, 0.0f32, 0.0f32);
    for o in 0..octaves {
        sum += amp * value_noise(x * freq, y * freq, seed.wrapping_add(o.wrapping_mul(0x9E37)));
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Angle (radians, from the tile center) of the point's NEAREST point on the rounded-rect boundary.
/// It is constant along each inward normal strip — on a straight side the nearest boundary point is
/// fixed as you move inward, and in a corner it is the fixed arc point — so a cross-band strip keeps
/// one value (flat plateau, hidden edge), while the value sweeps smoothly and seamlessly around the
/// whole perimeter including the curved corners.
fn rim_angle(vx: f32, vy: f32, ox: f32, oy: f32) -> f32 {
    let c = TILE_SIZE / 2.0;
    let x = (ox - vx) - c; // centered tile-local
    let y = (oy - vy) - c;
    let (sx, sy) = (if x < 0.0 { -1.0 } else { 1.0 }, if y < 0.0 { -1.0 } else { 1.0 });
    let (px, py) = (x.abs(), y.abs());
    let r = CORNER_RADIUS;
    let cc = c - r; // corner-center offset in the folded quadrant
    let (qx, qy) = (px - cc, py - cc);
    let (nx, ny) = if qx > 0.0 && qy > 0.0 {
        let len = (qx * qx + qy * qy).sqrt().max(1e-6); // rounded corner: project onto the arc
        (cc + r * qx / len, cc + r * qy / len)
    } else if qx > qy {
        (c, py) // nearer the x-max straight edge
    } else {
        (px, c) // nearer the y-max straight edge
    };
    (sy * ny).atan2(sx * nx) // unfold and take the angle of the nearest boundary point
}

/// Seamless noise around the rim: sample on a circle at the rim angle, so there is no corner seam.
fn rim_noise(theta: f32, seed: u32, period: f32) -> f32 {
    fbm(theta.cos() * RIM_CIRCLE_R, theta.sin() * RIM_CIRCLE_R, period, seed, 3)
}

/// The bare basin height at (vx,vy): an edge-distance basin bias with seeded noise layered on.
/// Floor undulates gently; the ramp has faded hillside bumps; the crest is a flat per-rim plateau
/// whose height undulates along the perimeter but is clamped to CREST_MIN so no saddle dips low.
/// [`surface_z`] carves the dug lake bowl on top of this.
fn basin_z(vx: f32, vy: f32, ox: f32, oy: f32) -> f32 {
    let ed = edge_dist(vx, vy, ox, oy);
    let theta = rim_angle(vx, vy, ox, oy);
    let band = BAND + BAND_VAR * rim_noise(theta, 0x00B4, BAND_RIM_PERIOD);
    let floor = FLOOR_Z + FLOOR_NOISE_AMP * fbm(vx, vy, FLOOR_PERIOD, 0x00F1, 3);
    if ed >= band {
        return floor; // gently undulating interior floor
    }
    let crest = (CREST_CENTER + CREST_AMP * rim_noise(theta, 0x00C3, RIDGE_PERIOD)).max(CREST_MIN);
    if ed <= CREST_INSET {
        return crest; // flat plateau per rim position -> hides the cut tile edge
    }
    // Steep hill ramp, floor -> crest; bumps fade to 0 at both seams so the plateau stays flat and
    // the floor stays continuous.
    let t = ((band - ed) / (band - CREST_INSET)).clamp(0.0, 1.0);
    let s = fade(t);
    let bump = RAMP_BUMP_AMP * (4.0 * s * (1.0 - s)) * fbm(vx, vy, HILL_PERIOD, 0x0077, 3);
    floor + (crest - floor) * s + bump
}

/// How far the dug lake bowl lowers the terrain at (vx,vy): a smooth dish, `LAKE_DEPTH` deep at the
/// centre easing to 0 at `LAKE_RADIUS` (smoothstep), and exactly 0 outside — the rest of the basin is
/// untouched. Subtracted from [`basin_z`] by [`surface_z`].
fn lake_bed_drop(vx: f32, vy: f32) -> f32 {
    let r = ((vx - LAKE_X).powi(2) + (vy - LAKE_Y).powi(2)).sqrt();
    if r >= LAKE_RADIUS {
        return 0.0;
    }
    let t = 1.0 - r / LAKE_RADIUS; // 1 at centre, 0 at rim
    LAKE_DEPTH * (t * t * (3.0 - 2.0 * t)) // smoothstep
}

/// World terrain height at (vx,vy): the basin with the lake bowl carved in. Every height consumer
/// (MCVT, normals, the MCLQ bed sampling, the verify) goes through here, so the bed and its shaded
/// slopes are real geometry.
fn surface_z(vx: f32, vy: f32, ox: f32, oy: f32) -> f32 {
    basin_z(vx, vy, ox, oy) - lake_bed_drop(vx, vy)
}

/// The MCLQ liquid block (804 bytes) for the chunk whose NW corner is (wx,wy), or `None` when no cell
/// sits under `LAKE_FILL_Z`. A flat still-water surface: a 9×9 grid all at the fill height, a
/// per-vertex depth byte from (fill − bed) for the shore ramp, and 8×8 cell nibbles 0 (still water)
/// where wet / 0xf dry. Vertex (row,col) is at (wx−row·UNIT, wy−col·UNIT), matching benilla's mesher.
fn build_mclq(wx: f32, wy: f32, ox: f32, oy: f32) -> Option<Vec<u8>> {
    let bed = |r: f32, c: f32| surface_z(wx - r * UNIT_SIZE, wy - c * UNIT_SIZE, ox, oy);
    let mut cell_wet = [false; 64];
    let mut any = false;
    for row in 0..8usize {
        for col in 0..8usize {
            let wet = bed(row as f32 + 0.5, col as f32 + 0.5) < LAKE_FILL_Z; // cell-centre bed
            cell_wet[row * 8 + col] = wet;
            any |= wet;
        }
    }
    if !any {
        return None;
    }
    let mut block = Vec::with_capacity(0x324);
    block.extend_from_slice(&LAKE_FILL_Z.to_le_bytes()); // min_height
    block.extend_from_slice(&LAKE_FILL_Z.to_le_bytes()); // max_height
    for row in 0..9usize {
        for col in 0..9usize {
            let depth = (LAKE_FILL_Z - bed(row as f32, col as f32)).max(0.0);
            block.push((depth * 9.0).clamp(0.0, 255.0) as u8); // union[0] = depth byte (lake swatch V=byte/42)
            block.extend_from_slice(&[0u8; 3]); // union[1..4] (flow, unread by benilla)
            block.extend_from_slice(&LAKE_FILL_Z.to_le_bytes()); // absolute surface height
        }
    }
    for &wet in &cell_wet {
        block.push(if wet { 0x00 } else { 0x0f }); // low nibble: 0 still water, 0xf dry
    }
    block.extend_from_slice(&0u32.to_le_bytes()); // nFlow = 0
    block.extend_from_slice(&[0u8; 80]); // 2×40B flow data (unread)
    debug_assert_eq!(block.len(), 0x324);
    Some(block)
}

/// Per-vertex MCNR normal from the height field, as the i8 [x,y,z] the file stores (WoW X,Y,Z).
/// Flat ground gives [0,0,127]; a slope tilts the normal up-hill. Central differences of the field.
fn normal_bytes(vx: f32, vy: f32, ox: f32, oy: f32) -> [i8; 3] {
    let e = NORMAL_EPS;
    let dzdx = (surface_z(vx + e, vy, ox, oy) - surface_z(vx - e, vy, ox, oy)) / (2.0 * e);
    let dzdy = (surface_z(vx, vy + e, ox, oy) - surface_z(vx, vy - e, ox, oy)) / (2.0 * e);
    let (nx, ny, nz) = (-dzdx, -dzdy, 1.0);
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    let enc = |c: f32| (c / len * 127.0).round().clamp(-127.0, 127.0) as i8;
    [enc(nx), enc(ny), enc(nz)]
}

/// One MCNK terrain chunk: 128-byte header + MCVT (basin heights) + MCNR (field normals) + one MCLY
/// layer. `impassable` sets MCNK_IMPASSABLE (a full-chunk mover wall) for the outer hill ring.
fn build_mcnk(index_x: u32, index_y: u32, ox: f32, oy: f32, impassable: bool) -> Vec<u8> {
    // Chunk NW (max) corner in WoW [X north, Y west].
    let wx = ox - index_y as f32 * CHUNK_SIZE;
    let wy = oy - index_x as f32 * CHUNK_SIZE;

    // 145 vertices in benilla's stride-17 layout: each of rows 0..9 holds 9 outer verts (cols 0..9)
    // at idx row*17+col, then rows 0..8 hold 8 inner verts (cols 0..8) at idx row*17+9+col. MCVT
    // heights are relative to the chunk base (FLOOR_Z); MCNR normals come from the same field.
    let mut heights = [0f32; 145];
    let mut norms = [[0i8, 0, 127]; 145];
    for row in 0..9u32 {
        for col in 0..9u32 {
            let idx = (row * 17 + col) as usize;
            let (vx, vy) = (wx - row as f32 * UNIT_SIZE, wy - col as f32 * UNIT_SIZE);
            heights[idx] = surface_z(vx, vy, ox, oy) - FLOOR_Z;
            norms[idx] = normal_bytes(vx, vy, ox, oy);
        }
        if row < 8 {
            for col in 0..8u32 {
                let idx = (row * 17 + 9 + col) as usize;
                let vx = wx - (row as f32 * UNIT_SIZE + UNIT_SIZE / 2.0);
                let vy = wy - (col as f32 * UNIT_SIZE + UNIT_SIZE / 2.0);
                heights[idx] = surface_z(vx, vy, ox, oy) - FLOOR_Z;
                norms[idx] = normal_bytes(vx, vy, ox, oy);
            }
        }
    }

    let mut mcvt_payload = Vec::with_capacity(145 * 4);
    for z in heights {
        mcvt_payload.extend_from_slice(&z.to_le_bytes());
    }
    let mut mcnr_payload = Vec::with_capacity(448);
    for n in norms {
        mcnr_payload.extend_from_slice(&[n[0] as u8, n[1] as u8, n[2] as u8]);
    }
    mcnr_payload.extend_from_slice(&[0u8; 13]); // retail MCNR is 448 bytes (435 + 13 pad)
    let mut mcly_payload = Vec::with_capacity(16);
    mcly_payload.extend_from_slice(&0u32.to_le_bytes()); // texture_id = 0 (MTEX[0])
    mcly_payload.extend_from_slice(&0u32.to_le_bytes()); // flags = 0 (opaque base layer)
    mcly_payload.extend_from_slice(&0u32.to_le_bytes()); // offset_in_mcal = 0
    mcly_payload.extend_from_slice(&0u32.to_le_bytes()); // effect_id = 0

    let mcvt = iff(b"TVCM", &mcvt_payload);
    let mcnr = iff(b"RNCM", &mcnr_payload);
    let mcly = iff(b"YLCM", &mcly_payload);
    let mclq_block = build_mclq(wx, wy, ox, oy);
    let mclq = mclq_block.as_ref().map(|b| iff(b"QLCM", b));

    // Header offsets use the "+8 from the MCNK magic" convention (benilla's reader resolves both).
    let ofs_mcvt = 8 + 128u32;
    let ofs_mcnr = 8 + 128 + mcvt.len() as u32;
    let ofs_mcly = 8 + 128 + mcvt.len() as u32 + mcnr.len() as u32;
    let ofs_mclq = ofs_mcly + mcly.len() as u32;

    // MCNK_IMPASSABLE flags the outer ring; the 0x4 liquid header bit marks a chunk that carries
    // water (the actual type is each cell's nibble, not this bit). The lake is interior, never a ring.
    let mut flags = if impassable { benilla_adt::MCNK_IMPASSABLE } else { 0 };
    if mclq.is_some() {
        flags |= 0x4;
    }

    let mut h = vec![0u8; 128];
    put_u32(&mut h, 0x00, flags);
    put_u32(&mut h, 0x04, index_x);
    put_u32(&mut h, 0x08, index_y);
    put_u32(&mut h, 0x0C, 1); // nLayers (reader ignores; retail-correct)
    put_u32(&mut h, 0x14, ofs_mcvt);
    put_u32(&mut h, 0x18, ofs_mcnr);
    put_u32(&mut h, 0x1C, ofs_mcly);
    if let Some(b) = &mclq_block {
        put_u32(&mut h, 0x60, ofs_mclq); // ofsMCLQ
        put_u32(&mut h, 0x64, b.len() as u32); // sizeLiquid — benilla reads the MCLQ length from here
    }
    // areaid (0x34), holes (0x3C), MCAL/MCSH offsets, pred_tex etc. all stay 0.
    // position = the chunk's NW (max) corner, WoW [X north, Y west, Z up]; base Z = FLOOR_Z.
    put_f32(&mut h, 0x68, wx);
    put_f32(&mut h, 0x6C, wy);
    put_f32(&mut h, 0x70, FLOOR_Z);

    let mut data = h;
    data.extend_from_slice(&mcvt);
    data.extend_from_slice(&mcnr);
    data.extend_from_slice(&mcly);
    if let Some(m) = &mclq {
        data.extend_from_slice(m);
    }
    iff(b"KNCM", &data)
}

/// Whether chunk (index_x,index_y) is impassable: its CLOSEST approach to the rounded boundary (min
/// edge_dist over the footprint) falls in the hill band (below IMPASSABLE_ED). The blocked region
/// therefore follows the rounded outline and curves at the corners (chunk-granular).
fn chunk_impassable(index_x: u32, index_y: u32, ox: f32, oy: f32) -> bool {
    let wx = ox - index_y as f32 * CHUNK_SIZE; // chunk NW (max) corner
    let wy = oy - index_x as f32 * CHUNK_SIZE;
    let n = 8u32;
    let mut min_ed = f32::INFINITY;
    for i in 0..=n {
        for j in 0..=n {
            let vx = wx - i as f32 / n as f32 * CHUNK_SIZE;
            let vy = wy - j as f32 / n as f32 * CHUNK_SIZE;
            min_ed = min_ed.min(edge_dist(vx, vy, ox, oy));
        }
    }
    min_ed < IMPASSABLE_ED
}

/// Static-scenery chunks: MMDX (NUL-terminated model names), MMID (their offsets) and MDDF
/// (placements). Each doodad's Z is the terrain height (`surface_z`) at its world (x,y); MDDF
/// positions are corner-origin (benilla `terrain.rs`): stored as [32·TILE−Y, Z, 32·TILE−X].
fn build_doodad_chunks(ox: f32, oy: f32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (mut mmdx, mut mmid, mut mddf) = (Vec::new(), Vec::new(), Vec::new());
    for (i, &(path, wx, wy, scale)) in DOODADS.iter().enumerate() {
        mmid.extend_from_slice(&(mmdx.len() as u32).to_le_bytes()); // offset of this name
        mmdx.extend_from_slice(path.as_bytes());
        mmdx.push(0); // NUL
        let z = surface_z(wx, wy, ox, oy);
        let pos = [MAP_OFFSET - wy, z, MAP_OFFSET - wx]; // corner-origin MDDF coords
        mddf.extend_from_slice(&(i as u32).to_le_bytes()); // name_id (MMID index)
        mddf.extend_from_slice(&(1_000_001 + i as u32).to_le_bytes()); // unique_id
        for v in pos {
            mddf.extend_from_slice(&v.to_le_bytes());
        }
        for r in [0.0f32; 3] {
            mddf.extend_from_slice(&r.to_le_bytes()); // rotation (degrees)
        }
        mddf.extend_from_slice(&((scale * 1024.0) as u16).to_le_bytes()); // scale ×1024
        mddf.extend_from_slice(&0u16.to_le_bytes()); // flags
    }
    (iff(b"XDMM", &mmdx), iff(b"DIMM", &mmid), iff(b"FDDM", &mddf))
}

/// Building chunks: MWMO (NUL-terminated WMO root names), MWID (their offsets) and MODF (placements,
/// 64 bytes each). benilla reads name_id@0, unique_id@4, pos@8, rotation@20 (degrees, Y = heading),
/// flags@56 and the doodad/name sets@58/60; it ignores the bounding box at 32..56, so we zero it.
/// Positions are corner-origin like MDDF (benilla `terrain.rs` MODF path): [32·TILE−Y, Z, 32·TILE−X].
fn build_wmo_chunks(ox: f32, oy: f32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (mut mwmo, mut mwid, mut modf) = (Vec::new(), Vec::new(), Vec::new());
    // Tuning knob: `WMO_SINK=<yards>` overrides every building's sink for quick eyeballing.
    let sink_override: Option<f32> = std::env::var("WMO_SINK").ok().and_then(|s| s.parse().ok());
    for (i, &(path, wx, wy, yaw, sink)) in WMOS.iter().enumerate() {
        let sink = sink_override.unwrap_or(sink);
        mwid.extend_from_slice(&(mwmo.len() as u32).to_le_bytes()); // offset of this name
        mwmo.extend_from_slice(path.as_bytes());
        mwmo.push(0); // NUL
        let z = surface_z(wx, wy, ox, oy) - sink; // bury the base/plinth (see WMOS doc)
        let pos = [MAP_OFFSET - wy, z, MAP_OFFSET - wx]; // corner-origin MODF coords
        modf.extend_from_slice(&(i as u32).to_le_bytes()); // name_id (MWID index)
        modf.extend_from_slice(&(2_000_001 + i as u32).to_le_bytes()); // unique_id
        for v in pos {
            modf.extend_from_slice(&v.to_le_bytes());
        }
        for r in [0.0f32, yaw, 0.0] {
            modf.extend_from_slice(&r.to_le_bytes()); // rotation degrees (Y = heading about up)
        }
        for _ in 0..6 {
            modf.extend_from_slice(&0.0f32.to_le_bytes()); // bounding box (unread by benilla)
        }
        modf.extend_from_slice(&0u16.to_le_bytes()); // flags
        modf.extend_from_slice(&0u16.to_le_bytes()); // doodad_set (0 = always-on set)
        modf.extend_from_slice(&0u16.to_le_bytes()); // name_set
        modf.extend_from_slice(&0u16.to_le_bytes()); // pad (scale, unused in 1.12)
    }
    (iff(b"OMWM", &mwmo), iff(b"DIWM", &mwid), iff(b"FDOM", &modf))
}

/// The whole ADT: MVER + MTEX + doodads (MMDX/MMID/MDDF) + 256 MCNK (row-major, cy outer).
fn build_adt(texture: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(iff(b"REVM", &18u32.to_le_bytes())); // MVER
    let mut mtex = texture.as_bytes().to_vec();
    mtex.push(0);
    out.extend(iff(b"XETM", &mtex)); // MTEX (one NUL-terminated name)
    let (ox, oy) = tile_to_world(TILE_X, TILE_Y);
    let (mmdx, mmid, mddf) = build_doodad_chunks(ox, oy);
    out.extend(mmdx); // MMDX (doodad model names)
    out.extend(mmid); // MMID (name offsets)
    out.extend(mddf); // MDDF (doodad placements)
    let (mwmo, mwid, modf) = build_wmo_chunks(ox, oy);
    out.extend(mwmo); // MWMO (WMO root names)
    out.extend(mwid); // MWID (name offsets)
    out.extend(modf); // MODF (WMO placements)
    for cy in 0..16u32 {
        for cx in 0..16u32 {
            out.extend(build_mcnk(cx, cy, ox, oy, chunk_impassable(cx, cy, ox, oy)));
        }
    }
    out
}

/// The WDT: MVER + MPHD (bit 0 clear = terrain map) + MAIN (one tile present).
fn build_wdt() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(iff(b"REVM", &18u32.to_le_bytes()));
    out.extend(iff(b"DHPM", &[0u8; 32])); // MPHD: no "one WMO" flag
    let mut main = vec![0u8; 64 * 64 * 8];
    let idx = (TILE_Y as usize * 64 + TILE_X as usize) * 8;
    main[idx] = 0x1; // flags bit 0 = has_adt
    out.extend(iff(b"NIAM", &main));
    out
}

/// Append one row (ID → directory, reused as MapName) to a real Map.dbc, preserving every record
/// and the string block verbatim.
fn build_map_dbc(original: &[u8], map_id: u32, dir: &str) -> Result<Vec<u8>> {
    ensure!(
        original.len() >= 20 && &original[0..4] == b"WDBC",
        "Map.dbc is not a WDBC file"
    );
    let rc = u32::from_le_bytes(original[4..8].try_into()?);
    let fc = u32::from_le_bytes(original[8..12].try_into()?);
    let rs = u32::from_le_bytes(original[12..16].try_into()?);
    let ss = u32::from_le_bytes(original[16..20].try_into()?);
    ensure!(
        fc == 42 && rs == 168,
        "unexpected Map.dbc shape (fields={fc}, record_size={rs}); schema expects 42 / 168"
    );
    let rec_len = rc as usize * rs as usize;
    let recs_end = 20 + rec_len;
    let str_end = recs_end + ss as usize;
    ensure!(original.len() >= str_end, "Map.dbc truncated");
    let records = &original[20..recs_end];
    let strings = &original[recs_end..str_end];

    // Append "<dir>\0" to the string block; the record points its Directory + MapName at it.
    let dir_off = strings.len() as u32;
    let mut new_strings = strings.to_vec();
    new_strings.extend_from_slice(dir.as_bytes());
    new_strings.push(0);

    let mut rec = vec![0u8; 168];
    put_u32(&mut rec, 0, map_id); // field 0: ID
    put_u32(&mut rec, 4, dir_off); // field 1: Directory
    put_u32(&mut rec, 8, 0); // field 2: InstanceType = 0 (a continent-style map)
    put_u32(&mut rec, 16, dir_off); // field 4: MapName (enUS) — reuse "Sandbox"
                                    // field 19 AreaTableID, field 38 LoadingScreenID, rest = 0.

    let mut out = Vec::new();
    out.extend_from_slice(b"WDBC");
    out.extend_from_slice(&(rc + 1).to_le_bytes());
    out.extend_from_slice(&fc.to_le_bytes());
    out.extend_from_slice(&rs.to_le_bytes());
    out.extend_from_slice(&(new_strings.len() as u32).to_le_bytes());
    out.extend_from_slice(records);
    out.extend_from_slice(&rec);
    out.extend_from_slice(&new_strings);
    Ok(out)
}

/// A 42-field / 168-byte schema sufficient to read ID (field 0) and Directory (field 1).
fn verify_schema() -> Schema {
    let mut s = Schema::new("Map");
    s.add_field(SchemaField::new("ID", FieldType::UInt32));
    s.add_field(SchemaField::new("Directory", FieldType::String));
    for i in 2..42 {
        s.add_field(SchemaField::new(format!("_p{i}"), FieldType::UInt32));
    }
    s
}

/// The gate: every file must parse through benilla's own readers and hold what we meant.
fn verify(adt: &[u8], wdt: &[u8], map_dbc: &[u8], texture: &str) -> Result<()> {
    // --- ADT via benilla-adt ---
    let benilla_adt::ParsedAdt::Root(root) =
        benilla_adt::parse_adt(&mut Cursor::new(adt)).context("parse_adt")?;
    ensure!(root.mcnk_chunks.len() == 256, "expected 256 MCNK, got {}", root.mcnk_chunks.len());
    ensure!(root.textures == [texture.to_string()], "MTEX mismatch: {:?}", root.textures);
    ensure!(
        root.doodad_placements.len() == DOODADS.len() && root.models.len() == DOODADS.len(),
        "doodad round-trip: {} placements / {} models, want {}",
        root.doodad_placements.len(),
        root.models.len(),
        DOODADS.len()
    );
    ensure!(
        root.wmo_placements.len() == WMOS.len() && root.wmos.len() == WMOS.len(),
        "wmo round-trip: {} placements / {} names, want {}",
        root.wmo_placements.len(),
        root.wmos.len(),
        WMOS.len()
    );
    let (ox, oy) = tile_to_world(TILE_X, TILE_Y);
    let mut n_impassable = 0usize;
    for (i, c) in root.mcnk_chunks.iter().enumerate() {
        let h = c.heights.as_ref().with_context(|| format!("chunk {i} MCVT"))?;
        ensure!(h.heights.len() == 145, "chunk {i} has {} heights", h.heights.len());
        let l = c.layers.as_ref().with_context(|| format!("chunk {i} MCLY"))?;
        ensure!(l.layers.len() == 1 && l.layers[0].texture_id == 0, "chunk {i} layers wrong");
        ensure!(c.header.position[2] == FLOOR_Z, "chunk {i} base height {}", c.header.position[2]);
        // (c) a chunk is impassable exactly when it enters the rounded hill band; interior is not.
        let (cx, cy) = (c.header.index_x, c.header.index_y);
        let want = chunk_impassable(cx, cy, ox, oy);
        ensure!(
            c.header.impassable() == want,
            "chunk ({cx},{cy}) impassable={} want {want}",
            c.header.impassable()
        );
        if c.header.impassable() {
            n_impassable += 1;
        }
    }
    // The blocked region must actually exist and must not swallow the whole tile (floor survives).
    ensure!(n_impassable > 0, "no impassable chunks flagged");
    ensure!(!chunk_impassable(8, 8, ox, oy), "centre chunk wrongly impassable");
    // --- ADT via the terrain mesher: the noisy basin profile holds end-to-end ---
    let mesh = benilla_formats::adt_to_tile_mesh(adt).context("adt_to_tile_mesh")?;
    ensure!(mesh.chunks.len() == 256, "mesh chunk count {}", mesh.chunks.len());
    // --- the dug lake: MCLQ must round-trip to real liquid surfaces ---
    let n_liquid_chunks = root.mcnk_chunks.iter().filter(|c| !c.liquids.is_empty()).count();
    ensure!(n_liquid_chunks > 0, "no MCLQ liquid chunks emitted — lake missing");
    let n_liquid_surfaces: usize = mesh.chunks.iter().map(|c| c.liquids.len()).sum();
    ensure!(n_liquid_surfaces > 0, "mesher produced no liquid surfaces from the lake");
    let floor_tol = FLOOR_NOISE_AMP + 0.6; // f32 round-trip slack over the noise amplitude
    let floor_test_ed = BAND + BAND_VAR + 3.0; // beyond any possible ramp start -> unambiguous floor
    let ring_ed = IMPASSABLE_ED; // edge_dist contour at the impassable region's inner face
    let (mut crest_min, mut crest_max) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut ring_inner_min = f32::INFINITY;
    let mut saw_floor = false;
    for ch in &mesh.chunks {
        for p in &ch.positions {
            let ed = edge_dist(p[0], p[1], ox, oy);
            // (a) the deep interior is a walkable low band around FLOOR_Z — except the dug lake bowl.
            if ed >= floor_test_ed && lake_bed_drop(p[0], p[1]) == 0.0 {
                ensure!(
                    (p[2] - FLOOR_Z).abs() <= floor_tol,
                    "interior vertex z {} outside floor band at edge_dist {ed:.1}",
                    p[2]
                );
                saw_floor = true;
            }
            // (b) crest band: collect the undulating ridge heights.
            if ed <= CREST_INSET {
                crest_min = crest_min.min(p[2]);
                crest_max = crest_max.max(p[2]);
            }
            // (e) the impassable ring's inner face: lowest terrain there must stay well above floor.
            if (ed - ring_ed).abs() <= UNIT_SIZE {
                ring_inner_min = ring_inner_min.min(p[2]);
            }
        }
    }
    ensure!(saw_floor, "no interior floor vertices sampled");
    ensure!(crest_min.is_finite() && crest_max.is_finite(), "no crest vertices sampled");
    ensure!(ring_inner_min.is_finite(), "no ring-inner-face vertices sampled");
    // (b) the crest stays unclimbable everywhere AND genuinely varies.
    ensure!(crest_min >= CREST_MIN - 0.1, "crest min {crest_min:.1} below wall threshold {CREST_MIN}");
    ensure!(crest_max > crest_min + 1.0, "crest not varied (min {crest_min:.1}, max {crest_max:.1})");
    // (e) no low saddle / walkable gap at the wall's inner face -> the void edge is never exposed.
    ensure!(
        ring_inner_min >= RING_INNER_MIN,
        "ring inner-face min {ring_inner_min:.1} below safe threshold {RING_INNER_MIN}"
    );
    // (d) the spawn point (tile centre) sits on the walkable floor.
    let (scx, scy) = (ox - TILE_SIZE / 2.0, oy - TILE_SIZE / 2.0);
    let sz = surface_z(scx, scy, ox, oy);
    ensure!((sz - FLOOR_Z).abs() <= floor_tol, "spawn ({scx:.1},{scy:.1}) not on floor: z {sz:.2}");

    // (f) the footprint is actually ROUNDED. A point near a corner and a mid-edge point BOTH sit the
    // same perpendicular distance (40 yd) from their nearest straight edge; with a sharp rectangle
    // both would have edge_dist 40 (same height). Rounding pulls the corner point far closer to the
    // boundary (deep into the hill, up at the crest), while the mid-edge point stays on the ramp —
    // so the corner curves inward. Compare via tile-local coords (lx,ly), world = (ox-lx, oy-ly).
    let at = |lx: f32, ly: f32| {
        let (vx, vy) = (ox - lx, oy - ly);
        (edge_dist(vx, vy, ox, oy), surface_z(vx, vy, ox, oy))
    };
    let (corner_ed, corner_z) = at(40.0, 40.0); // ~40 yd from each straight edge
    let (edge_ed, edge_z) = at(40.0, TILE_SIZE / 2.0); // 40 yd from one edge, mid-side
    ensure!(
        corner_ed <= CREST_INSET,
        "corner (40,40) edge_dist {corner_ed:.1} not inside the crest band — footprint not rounded"
    );
    ensure!(
        edge_ed > CREST_INSET + 5.0,
        "mid-edge (40,mid) edge_dist {edge_ed:.1} unexpectedly at the crest — edges should stay straight"
    );
    ensure!(
        corner_ed < edge_ed - 20.0,
        "corner edge_dist {corner_ed:.1} not meaningfully inside mid-edge {edge_ed:.1} — not rounded"
    );
    ensure!(corner_z > FLOOR_Z + 25.0, "corner (40,40) z {corner_z:.1} not up in the hill band");

    println!(
        "  basin profile: floor ~{FLOOR_Z}±{FLOOR_NOISE_AMP}, crest min {crest_min:.1}/max {crest_max:.1} (wall ≥{CREST_MIN}), ring inner-face min {ring_inner_min:.1} (≥{RING_INNER_MIN}), spawn z {sz:.2}"
    );
    println!(
        "  rounded footprint: CORNER_RADIUS={CORNER_RADIUS}, {n_impassable} impassable chunks; corner(40,40) edge_dist {corner_ed:.1} z {corner_z:.1} (crest) vs mid-edge(40,mid) edge_dist {edge_ed:.1} z {edge_z:.1} (ramp)"
    );

    // --- WDT via benilla-wdt ---
    let wf = benilla_wdt::WdtReader::new(Cursor::new(wdt), benilla_wdt::WowVersion::Classic)
        .read()
        .context("WDT read")?;
    ensure!(wf.get_tile(TILE_X as usize, TILE_Y as usize).unwrap().has_adt, "tile not present");
    ensure!(!wf.get_tile(0, 0).unwrap().has_adt, "stray tile present");
    ensure!(!wf.is_one_object(), "WDT wrongly marked WMO-only");

    // --- Map.dbc via benilla-dbc ---
    let parser = DbcParser::parse(&mut Cursor::new(map_dbc))
        .context("Map.dbc parse")?
        .with_schema(verify_schema())
        .context("Map.dbc schema")?;
    let rs = parser.parse_records().context("Map.dbc records")?;
    let mut found_new = false;
    let mut found_azeroth = false;
    for r in rs.records() {
        let Some(Value::UInt32(id)) = r.get_value(0) else { continue };
        if *id == MAP_ID {
            if let Some(Value::StringRef(StringRef(off))) = r.get_value(1) {
                let dir = rs.get_string(StringRef(*off))?;
                ensure!(dir == FOLDER, "new map directory {dir:?} != {FOLDER:?}");
                found_new = true;
            }
        }
        if *id == 0 {
            found_azeroth = true; // an original map still resolves
        }
    }
    ensure!(found_new, "new map id {MAP_ID} not found after append");
    ensure!(found_azeroth, "original map id 0 missing after append");
    Ok(())
}

/// Stage an overlay Data dir: symlink every real-Data entry in, then write the three loose files.
fn stage(real_data: &Path, overlay: &Path, adt: &[u8], wdt: &[u8], map_dbc: &[u8]) -> Result<()> {
    std::fs::create_dir_all(overlay).with_context(|| format!("mkdir {}", overlay.display()))?;
    // Symlink the real install's archives/subdirs in (non-destructive; real install untouched).
    for entry in std::fs::read_dir(real_data).with_context(|| format!("read {}", real_data.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        // Our loose dirs are DBFilesClient/ and World/ — real Data has neither (they live in MPQs).
        let link = overlay.join(&name);
        if link.exists() {
            continue;
        }
        let target = std::fs::canonicalize(entry.path())?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)
            .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
    }
    let write = |rel: &str, bytes: &[u8]| -> Result<()> {
        let p = overlay.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, bytes).with_context(|| format!("write {}", p.display()))?;
        Ok(())
    };
    write("DBFilesClient/Map.dbc", map_dbc)?;
    write(&format!("World/Maps/{FOLDER}/{FOLDER}.wdt"), wdt)?;
    write(&format!("World/Maps/{FOLDER}/{FOLDER}_{TILE_X}_{TILE_Y}.adt"), adt)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // `height <x> <y> [<x> <y> ...]`: print the map-800 terrain height (basin + lake) at each world
    // (x,y), for placing DB content (NPCs, GOs) on the ground instead of a guessed fixed z. Prints
    // one `x y z` line per pair; uses this tile's origin, so it matches what mapgen bakes.
    if args.len() >= 4 && args[1] == "height" {
        let coords = &args[2..];
        if coords.len() % 2 != 0 {
            bail!("usage: warcraft-mapgen height <x> <y> [<x> <y> ...]");
        }
        let (ox, oy) = tile_to_world(TILE_X, TILE_Y);
        for pair in coords.chunks(2) {
            let x: f32 = pair[0].parse().with_context(|| format!("bad x {:?}", pair[0]))?;
            let y: f32 = pair[1].parse().with_context(|| format!("bad y {:?}", pair[1]))?;
            println!("{x:.3} {y:.3} {:.4}", surface_z(x, y, ox, oy));
        }
        return Ok(());
    }
    // `servermap <out.map>`: emit the vmangos SERVER-side terrain-height file for map 800 tile 32,32
    // (`maps/8003232.map`), sampled from `surface_z` so the server's GetHeight matches the client.
    // Without this the server has no height on map 800 (GetHeight -> -200000) and any creature that
    // MOVES hangs the world thread. Format: GridMapFileHeader(40) + MHGT header(16) + V9(129x129 f32)
    // + V8(128x128 f32), all little-endian, matching GridMap::loadHeightData / getHeightFromFloat.
    if args.len() == 3 && args[1] == "servermap" {
        let out = PathBuf::from(&args[2]);
        let (ox, oy) = tile_to_world(TILE_X, TILE_Y);
        let unit = TILE_SIZE / 128.0; // yards between grid samples (≈4.1667)
        let (mut hmin, mut hmax) = (f32::INFINITY, f32::NEG_INFINITY);
        let mut v9 = Vec::with_capacity(129 * 129); // corners, X outer (stride 129), Y inner
        for i in 0..=128usize {
            for j in 0..=128usize {
                let h = surface_z(ox - i as f32 * unit, oy - j as f32 * unit, ox, oy);
                hmin = hmin.min(h);
                hmax = hmax.max(h);
                v9.push(h);
            }
        }
        let mut v8 = Vec::with_capacity(128 * 128); // cell centres
        for i in 0..128usize {
            for j in 0..128usize {
                let h = surface_z(ox - (i as f32 + 0.5) * unit, oy - (j as f32 + 0.5) * unit, ox, oy);
                hmin = hmin.min(h);
                hmax = hmax.max(h);
                v8.push(h);
            }
        }
        let height_size = 16 + (v9.len() + v8.len()) * 4;
        let mut buf = Vec::with_capacity(40 + height_size);
        buf.extend_from_slice(b"MAPS"); // mapMagic
        buf.extend_from_slice(b"z1.4"); // versionMagic
        buf.extend_from_slice(&0u32.to_le_bytes()); // areaMapOffset (0 = skip; area 0)
        buf.extend_from_slice(&0u32.to_le_bytes()); // areaMapSize
        buf.extend_from_slice(&40u32.to_le_bytes()); // heightMapOffset
        buf.extend_from_slice(&(height_size as u32).to_le_bytes()); // heightMapSize
        buf.extend_from_slice(&0u32.to_le_bytes()); // liquidMapOffset (0 = skip)
        buf.extend_from_slice(&0u32.to_le_bytes()); // liquidMapSize
        buf.extend_from_slice(&0u32.to_le_bytes()); // holesOffset (0 = skip)
        buf.extend_from_slice(&0u32.to_le_bytes()); // holesSize
        buf.extend_from_slice(b"MHGT"); // fourcc
        buf.extend_from_slice(&0u32.to_le_bytes()); // flags = 0 (float grid, has height)
        buf.extend_from_slice(&hmin.to_le_bytes()); // gridHeight
        buf.extend_from_slice(&hmax.to_le_bytes()); // gridMaxHeight
        for h in v9.iter().chain(v8.iter()) {
            buf.extend_from_slice(&h.to_le_bytes());
        }
        std::fs::write(&out, &buf).with_context(|| format!("write {}", out.display()))?;
        println!(
            "wrote {} ({} bytes) — map 800 tile 32,32 server height, {:.2}..{:.2} yd",
            out.display(),
            buf.len(),
            hmin,
            hmax
        );
        return Ok(());
    }
    if args.len() != 3 {
        bail!("usage: warcraft-mapgen <real-Data-dir> <overlay-Data-dir>  |  height <x> <y> ...  |  servermap <out.map>");
    }
    let real_data = PathBuf::from(&args[1]);
    let overlay = PathBuf::from(&args[2]);
    ensure!(real_data.is_dir(), "real Data dir not found: {}", real_data.display());

    // Pick a ground texture that exists in the install.
    let mut chain = benilla_formats::open_chain(&real_data).context("open real Data chain")?;
    let texture = TEXTURE_CANDIDATES
        .iter()
        .find(|t| chain.contains(t))
        .copied()
        .unwrap_or_else(|| {
            eprintln!("warning: none of the candidate textures were found; terrain will render white");
            TEXTURE_CANDIDATES[0]
        })
        .to_string();

    let original_map = chain
        .read_file("DBFilesClient\\Map.dbc")
        .context("reading the install's Map.dbc")?;

    let adt = build_adt(&texture);
    let wdt = build_wdt();
    let map_dbc = build_map_dbc(&original_map, MAP_ID, FOLDER)?;

    verify(&adt, &wdt, &map_dbc, &texture).context("ROUND-TRIP VERIFY failed")?;
    println!("round-trip VERIFY passed (ADT 256 basin chunks, WDT tile {TILE_X},{TILE_Y}, Map.dbc new row {MAP_ID})");

    stage(&real_data, &overlay, &adt, &wdt, &map_dbc)?;

    // Runtime-path check: the streamer loads terrain from `mpq-loose://`, i.e. LooseIndex::resolve
    // with LooseOpen::Index over (WOW_DATA.parent(), WOW_DATA). Prove it resolves each staged file.
    {
        use benilla_formats::{LooseIndex, LooseOpen};
        let idx = LooseIndex::build(overlay.parent(), &overlay);
        for (rel, bytes) in [
            (format!("World\\Maps\\{FOLDER}\\{FOLDER}.wdt"), &wdt),
            (format!("World\\Maps\\{FOLDER}\\{FOLDER}_{TILE_X}_{TILE_Y}.adt"), &adt),
            ("DBFilesClient\\Map.dbc".to_string(), &map_dbc),
        ] {
            let f = idx
                .resolve(&rel, LooseOpen::Index)
                .with_context(|| format!("mpq-loose:// did NOT resolve {rel}"))?;
            let got = f.read().with_context(|| format!("reading loose {rel}"))?;
            ensure!(got == *bytes, "loose {rel} bytes differ from generated");
        }
        println!("mpq-loose:// resolves all three staged files (WDT, ADT, Map.dbc) — the fixed streamer will find them");
    }

    // The decisive gate: build the MapCatalog the way world_map does (`load_map_catalog`, now
    // loose-aware) and confirm map 800 resolves — i.e. stream_terrain's `directory(map_id)` check
    // (terrain_stream.rs L636) now passes instead of returning early.
    std::env::set_var("WOW_DATA", overlay.as_os_str()); // so install_index() reads loose from the overlay
    let mut ov_chain = benilla_formats::open_chain(&overlay).context("open overlay chain")?;
    let cat = benilla_formats::load_map_catalog(&mut ov_chain).context("load_map_catalog")?;
    ensure!(
        cat.directory(MAP_ID) == Some(FOLDER),
        "catalog directory({MAP_ID}) = {:?}, want Some({FOLDER:?}) — streamer would still bail at L636",
        cat.directory(MAP_ID)
    );
    ensure!(cat.directory(0).is_some(), "original map 0 missing from the loose-aware catalog");
    println!(
        "MapCatalog (loose-aware) = {} maps; directory({MAP_ID}) -> {:?} — stream_terrain passes its directory gate",
        cat.len(),
        cat.directory(MAP_ID)
    );

    let (ox, oy) = tile_to_world(TILE_X, TILE_Y);
    let cx = ox - TILE_SIZE / 2.0;
    let cy = oy - TILE_SIZE / 2.0;
    let sz = FLOOR_Z + 2.0;
    println!("texture: {texture}");
    println!("overlay Data: {}", overlay.display());
    println!("ADT: {} bytes   WDT: {} bytes   Map.dbc: {} bytes", adt.len(), wdt.len(), map_dbc.len());
    println!("spawn (tile centre): WOW_EXPLORE_AT={MAP_ID},{cx:.3},{cy:.3},{sz:.1}");
    Ok(())
}
