//! A from-scratch reader for vmangos's Detour navigation meshes (the `mmaps/` the server generated
//! for creature pathfinding), so the bot can route around obstacles instead of straight-lining.
//!
//! File formats (standard TrinityCore/vmangos, Detour `DT_NAVMESH_VERSION` 7):
//! - `<map:03>.mmap` — a `dtNavMeshParams`: origin[3] f32, tileWidth f32, tileHeight f32,
//!   maxTiles i32, maxPolys i32 (28 bytes).
//! - `<map:03><tx:02><ty:02>.mmtile` — a 20-byte `MmapTileHeader` (magic `MMAP`, dtVersion,
//!   mmapVersion, size, usesLiquids+pad) then `size` bytes of raw `dtMeshTile` data (which begins
//!   with the `DNAV` `dtMeshHeader`), optionally followed by liquid data we ignore.
//!
//! This module (a) parses a tile into polygons/vertices (+ detail mesh for height) with a
//! size self-check. A*, the funnel, and cross-tile links build on it.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

// Detour/mmap magics are packed big-endian-style ('D'<<24|'N'<<16|'A'<<8|'V'), so a little-endian
// file reads them back as these values.
const DT_NAVMESH_MAGIC: i32 = i32::from_be_bytes(*b"DNAV");
const MMAP_MAGIC: u32 = u32::from_be_bytes(*b"MMAP");
const DT_NAVMESH_VERSION: i32 = 7;
const DT_VERTS_PER_POLYGON: usize = 6;

/// `dtPoly` serialized size (firstLink u32, verts[6] u16, neis[6] u16, flags u16, vertCount u8,
/// areaAndtype u8).
const SIZEOF_POLY: usize = 4 + 2 * DT_VERTS_PER_POLYGON + 2 * DT_VERTS_PER_POLYGON + 2 + 1 + 1;
/// `dtLink` (ref dtPolyRef, next u32, edge u8, side u8, bmin u8, bmax u8). vmangos builds Detour
/// with a 64-bit `dtPolyRef` (`DT_POLYREF64`), so `ref` is 8 bytes → 16 total.
const SIZEOF_LINK: usize = 8 + 4 + 1 + 1 + 1 + 1;
/// `dtPolyDetail` (vertBase u32, triBase u32, vertCount u8, triCount u8, +2 pad).
const SIZEOF_POLY_DETAIL: usize = 4 + 4 + 1 + 1 + 2;
/// `dtBVNode` (bmin[3] u16, bmax[3] u16, i i32).
const SIZEOF_BVNODE: usize = 2 * 3 + 2 * 3 + 4;
/// `dtOffMeshConnection` (pos[6] f32, rad f32, poly u16, flags u8, side u8, userId u32).
const SIZEOF_OFFMESH_CON: usize = 4 * 6 + 4 + 2 + 1 + 1 + 4;

/// `Map.dbc`-less per-map nav params from a `.mmap` file.
#[derive(Debug, Clone, Copy)]
pub struct NavMeshParams {
    pub origin: [f32; 3],
    pub tile_width: f32,
    pub tile_height: f32,
    pub max_tiles: i32,
    pub max_polys: i32,
}

/// One polygon: its vertex indices (into the tile's `verts`) and its area/type byte. Detour packs
/// area (low 6 bits) and type (high 2 bits); type 1 is an off-mesh connection.
#[derive(Debug, Clone)]
pub struct Poly {
    pub verts: Vec<u16>,
    /// Per-edge neighbor codes (edge `k` joins vert `k`→`k+1`). `0` = solid border; the high bit
    /// (`0x8000`, `DT_EXT_LINK`) = a portal to an adjacent tile (resolved via the links array, not
    /// yet parsed); otherwise `code - 1` is the neighbor poly's index *in this tile*.
    pub neis: Vec<u16>,
    pub area: u8,
    pub poly_type: u8,
}

/// A parsed nav-mesh tile: enough to walk the polygon graph and read heights.
#[derive(Debug)]
pub struct MeshTile {
    pub x: i32,
    pub y: i32,
    pub bmin: [f32; 3],
    pub bmax: [f32; 3],
    pub verts: Vec<[f32; 3]>,
    pub polys: Vec<Poly>,
}

impl MeshTile {
    /// Polygon `i`'s centroid in the Recast frame (mean of its vertices).
    fn centroid(&self, i: usize) -> [f32; 3] {
        let p = &self.polys[i];
        let n = p.verts.len() as f32;
        let mut c = [0.0; 3];
        for &v in &p.verts {
            let vert = self.verts[v as usize];
            c[0] += vert[0];
            c[1] += vert[1];
            c[2] += vert[2];
        }
        [c[0] / n, c[1] / n, c[2] / n]
    }
}

fn dist3(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// An A* open-set entry, ordered so [`BinaryHeap`] (a max-heap) pops the lowest `f` first.
struct Open {
    f: f32,
    poly: usize,
}
impl PartialEq for Open {
    fn eq(&self, o: &Self) -> bool {
        self.f == o.f
    }
}
impl Eq for Open {}
impl Ord for Open {
    fn cmp(&self, o: &Self) -> Ordering {
        o.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal) // reversed → min-heap
    }
}
impl PartialOrd for Open {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// A little-endian byte cursor.
struct Reader<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, o: 0 }
    }
    fn need(&self, n: usize) -> Result<()> {
        if self.o + n > self.b.len() {
            bail!("navmesh: unexpected end of data at {} (+{n} > {})", self.o, self.b.len());
        }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.b[self.o];
        self.o += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        self.need(2)?;
        let v = u16::from_le_bytes([self.b[self.o], self.b[self.o + 1]]);
        self.o += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        self.need(4)?;
        let v = u32::from_le_bytes(self.b[self.o..self.o + 4].try_into().unwrap());
        self.o += 4;
        Ok(v)
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn vec3(&mut self) -> Result<[f32; 3]> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.o += n;
        Ok(())
    }
}

/// One ADT/Detour tile spans this many yards on a side (`TILESIZE`, 1600/3).
pub const TILE_SIZE: f32 = 533.333_3;

/// WoW world coords `[x, y, z]` → the Recast/Detour frame the mmaps are stored in. vmangos (like
/// TrinityCore) builds the navmesh with axes permuted and no sign flips: recast = `[wowY, wowZ,
/// wowX]` (recast Y is up). Verified against map 0's Northshire tile bounds.
pub fn wow_to_recast(p: [f32; 3]) -> [f32; 3] {
    [p[1], p[2], p[0]]
}

/// Recast/Detour coords → WoW world coords (the inverse of [`wow_to_recast`]).
pub fn recast_to_wow(p: [f32; 3]) -> [f32; 3] {
    [p[2], p[0], p[1]]
}

/// The `.mmtile` grid cell `(gx, gy)` whose square contains WoW `(x, y)`. The filename is
/// `{map:03}{gx:02}{gy:02}.mmtile`; gx comes from world X, gy from world Y.
pub fn tile_for(x: f32, y: f32) -> (i32, i32) {
    let gx = (32.0 - x / TILE_SIZE).floor() as i32;
    let gy = (32.0 - y / TILE_SIZE).floor() as i32;
    (gx.clamp(0, 63), gy.clamp(0, 63))
}

/// A stable handle to one polygon: which tile it is in and its index within that tile. Light and
/// `Copy` so A* can use it as a graph node without borrowing the tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PolyRef {
    pub gx: i32,
    pub gy: i32,
    pub poly: usize,
}

/// A loaded navmesh for one map: the `.mmap` params plus a lazy cache of parsed tiles (a cell maps
/// to `None` once we learn it has no tile file, so we never re-stat it).
pub struct NavMesh {
    pub map: u32,
    pub params: NavMeshParams,
    dir: PathBuf,
    tiles: HashMap<(i32, i32), Option<MeshTile>>,
}

impl NavMesh {
    /// Open a map's navmesh: read `{map:03}.mmap` from `dir` (the server's `mmaps/` directory).
    pub fn open(dir: impl AsRef<Path>, map: u32) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let path = dir.join(format!("{map:03}.mmap"));
        let params = parse_params(&fs::read(&path).with_context(|| format!("reading {path:?}"))?)
            .with_context(|| format!("parsing {path:?}"))?;
        Ok(Self {
            map,
            params,
            dir,
            tiles: HashMap::new(),
        })
    }

    /// The parsed tile for cell `(gx, gy)`, or `None` if the map has no tile there. Parsed once and
    /// cached; a parse error is treated as "no tile" (logged to stderr) so one bad tile can't abort
    /// routing.
    pub fn tile(&mut self, gx: i32, gy: i32) -> Option<&MeshTile> {
        self.tiles
            .entry((gx, gy))
            .or_insert_with(|| {
                let path = self.dir.join(format!("{:03}{gx:02}{gy:02}.mmtile", self.map));
                match fs::read(&path) {
                    Ok(bytes) => match parse_tile(&bytes) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            eprintln!("navmesh: skipping {path:?}: {e:#}");
                            None
                        }
                    },
                    Err(_) => None, // no tile file here (off the map, or unwalkable)
                }
            })
            .as_ref()
    }

    /// Locate the ground polygon under a WoW point: the walkable poly whose 2D footprint contains
    /// it, choosing the one closest in height when several overlap (bridges, ramps). Falls back to
    /// the nearest poly by centroid within the same tile when none strictly contains the point.
    /// Returns the [`PolyRef`] and the point snapped onto the mesh in WoW coords.
    pub fn find_poly(&mut self, wow: [f32; 3]) -> Option<(PolyRef, [f32; 3])> {
        let (gx, gy) = tile_for(wow[0], wow[1]);
        let r = wow_to_recast(wow);
        let tile = self.tile(gx, gy)?;

        let mut best_contained: Option<(usize, f32)> = None; // (poly, |dy|)
        let mut best_any: Option<(usize, f32)> = None; // (poly, 2D dist to centroid)
        for (i, poly) in tile.polys.iter().enumerate() {
            if poly.poly_type != 0 || poly.verts.len() < 3 {
                continue; // skip off-mesh connections and degenerate polys
            }
            let vs: Vec<[f32; 3]> = poly.verts.iter().map(|&v| tile.verts[v as usize]).collect();
            // Centroid distance (the always-available fallback), in the recast X/Z plane.
            let (mut cx, mut cz) = (0.0f32, 0.0f32);
            for v in &vs {
                cx += v[0];
                cz += v[2];
            }
            let n = vs.len() as f32;
            let d2 = (cx / n - r[0]).hypot(cz / n - r[2]);
            if best_any.map_or(true, |(_, bd)| d2 < bd) {
                best_any = Some((i, d2));
            }
            // Strict 2D containment; among those, prefer the nearest in height.
            if point_in_poly_xz(r, &vs) {
                let mean_y = vs.iter().map(|v| v[1]).sum::<f32>() / n;
                let dy = (mean_y - r[1]).abs();
                if best_contained.map_or(true, |(_, bdy)| dy < bdy) {
                    best_contained = Some((i, dy));
                }
            }
        }

        let poly = best_contained.or(best_any)?.0;
        // Snap height onto the chosen poly (mean vertex height), convert back to WoW.
        let p = &tile.polys[poly];
        let mean_y = p
            .verts
            .iter()
            .map(|&v| tile.verts[v as usize][1])
            .sum::<f32>()
            / p.verts.len() as f32;
        let snapped = recast_to_wow([r[0], mean_y, r[2]]);
        Some((PolyRef { gx, gy, poly }, snapped))
    }

    /// Route from `start` to `end` (WoW coords) over the polygon graph with A*, returning a list of
    /// WoW waypoints `[start, …, end]`. Intermediate waypoints are polygon centroids, so the route
    /// is correct but not yet straightened — the funnel (string-pulling) pass refines it next.
    ///
    /// Intra-tile only for now: if start and end fall in different `.mmtile` cells this returns
    /// `None` (cross-tile portal links are the next chunk). `None` also if either endpoint is off
    /// the mesh or no polygon path connects them.
    pub fn find_path(&mut self, start: [f32; 3], end: [f32; 3]) -> Option<Vec<[f32; 3]>> {
        let (s, _) = self.find_poly(start)?;
        let (e, _) = self.find_poly(end)?;
        if (s.gx, s.gy) != (e.gx, e.gy) {
            return None; // cross-tile routing not built yet
        }
        let tile = self.tile(s.gx, s.gy)?;
        let goal_c = tile.centroid(e.poly);

        // Standard A* over polys; step cost and heuristic are 3D centroid distances in Recast space.
        let mut open = BinaryHeap::new();
        let mut g: HashMap<usize, f32> = HashMap::new();
        let mut came: HashMap<usize, usize> = HashMap::new();
        g.insert(s.poly, 0.0);
        open.push(Open {
            f: dist3(tile.centroid(s.poly), goal_c),
            poly: s.poly,
        });

        while let Some(Open { poly, .. }) = open.pop() {
            if poly == e.poly {
                break;
            }
            let here = tile.centroid(poly);
            let g_here = g[&poly];
            for (k, &nei) in tile.polys[poly].neis.iter().enumerate() {
                // Skip borders (0) and cross-tile portals (0x8000) until links are parsed.
                if nei == 0 || nei & 0x8000 != 0 {
                    continue;
                }
                let next = (nei - 1) as usize;
                if next >= tile.polys.len() || tile.polys[next].poly_type != 0 {
                    continue;
                }
                let _ = k;
                let tentative = g_here + dist3(here, tile.centroid(next));
                if tentative < *g.get(&next).unwrap_or(&f32::INFINITY) {
                    came.insert(next, poly);
                    g.insert(next, tentative);
                    open.push(Open {
                        f: tentative + dist3(tile.centroid(next), goal_c),
                        poly: next,
                    });
                }
            }
        }

        if !came.contains_key(&e.poly) && s.poly != e.poly {
            return None; // no connected path
        }

        // Walk the came-from chain back to the start, then build WoW waypoints.
        let mut chain = vec![e.poly];
        let mut cur = e.poly;
        while cur != s.poly {
            cur = came[&cur];
            chain.push(cur);
        }
        chain.reverse();

        let mut waypoints = vec![start];
        for &poly in chain.iter().skip(1).take(chain.len().saturating_sub(2)) {
            waypoints.push(recast_to_wow(tile.centroid(poly)));
        }
        waypoints.push(end);
        Some(waypoints)
    }
}

/// Even-odd point-in-polygon test in the recast X/Z ground plane (recast Y is height, ignored).
fn point_in_poly_xz(p: [f32; 3], verts: &[[f32; 3]]) -> bool {
    let (px, pz) = (p[0], p[2]);
    let mut inside = false;
    let mut j = verts.len() - 1;
    for i in 0..verts.len() {
        let (xi, zi) = (verts[i][0], verts[i][2]);
        let (xj, zj) = (verts[j][0], verts[j][2]);
        if (zi > pz) != (zj > pz) && px < (xj - xi) * (pz - zi) / (zj - zi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Parse a `.mmap` file's `dtNavMeshParams`.
pub fn parse_params(bytes: &[u8]) -> Result<NavMeshParams> {
    let mut r = Reader::new(bytes);
    Ok(NavMeshParams {
        origin: r.vec3()?,
        tile_width: r.f32()?,
        tile_height: r.f32()?,
        max_tiles: r.i32()?,
        max_polys: r.i32()?,
    })
}

/// Parse a `.mmtile` file into a [`MeshTile`]. Validates the `MMAP`/`DNAV` magics, the Detour
/// version, and that the sections sum to the declared tile size (the layout self-check).
pub fn parse_tile(bytes: &[u8]) -> Result<MeshTile> {
    let mut r = Reader::new(bytes);
    // MmapTileHeader (20 bytes).
    let mmap_magic = r.u32().context("mmtile magic")?;
    if mmap_magic != MMAP_MAGIC {
        bail!("mmtile: bad magic {mmap_magic:#x}, expected MMAP");
    }
    let dt_version = r.i32()?;
    let _mmap_version = r.u32()?;
    let data_size = r.u32()? as usize;
    let _uses_liquids = r.u32()?; // bool + 3 pad
    if dt_version != DT_NAVMESH_VERSION {
        bail!("mmtile: Detour version {dt_version}, expected {DT_NAVMESH_VERSION}");
    }

    // The dtMeshTile data begins here; anything past `data_size` (liquid data) is ignored.
    let data_start = r.o;
    let nav = bytes
        .get(data_start..data_start + data_size)
        .context("mmtile: declared size exceeds the file")?;
    let mut d = Reader::new(nav);

    // dtMeshHeader.
    let magic = d.i32()?;
    if magic != DT_NAVMESH_MAGIC {
        bail!("mmtile: dtMeshHeader magic {magic:#x}, expected DNAV");
    }
    let _version = d.i32()?;
    let x = d.i32()?;
    let y = d.i32()?;
    let _layer = d.i32()?;
    let _user_id = d.u32()?;
    let poly_count = d.i32()? as usize;
    let vert_count = d.i32()? as usize;
    let max_link_count = d.i32()? as usize;
    let detail_mesh_count = d.i32()? as usize;
    let detail_vert_count = d.i32()? as usize;
    let detail_tri_count = d.i32()? as usize;
    let bv_node_count = d.i32()? as usize;
    let off_mesh_con_count = d.i32()? as usize;
    let _off_mesh_base = d.i32()?;
    let _walkable_height = d.f32()?;
    let _walkable_radius = d.f32()?;
    let _walkable_climb = d.f32()?;
    let bmin = d.vec3()?;
    let bmax = d.vec3()?;
    let _bv_quant = d.f32()?;
    let header_size = d.o; // 100 bytes

    // Sections, in dtCreateNavMeshData order.
    let mut verts = Vec::with_capacity(vert_count);
    for _ in 0..vert_count {
        verts.push(d.vec3()?);
    }
    let mut polys = Vec::with_capacity(poly_count);
    for _ in 0..poly_count {
        let _first_link = d.u32()?;
        let mut pv = [0u16; DT_VERTS_PER_POLYGON];
        for v in &mut pv {
            *v = d.u16()?;
        }
        let mut pn = [0u16; DT_VERTS_PER_POLYGON];
        for nv in &mut pn {
            *nv = d.u16()?;
        }
        let _flags = d.u16()?;
        let vc = d.u8()? as usize;
        let area_and_type = d.u8()?;
        let vc = vc.min(DT_VERTS_PER_POLYGON);
        polys.push(Poly {
            verts: pv[..vc].to_vec(),
            neis: pn[..vc].to_vec(),
            area: area_and_type & 0x3f,
            poly_type: area_and_type >> 6,
        });
    }
    // Skip the rest for now (links/detail/bv/offmesh); accounted for in the size check.
    d.skip(max_link_count * SIZEOF_LINK)?;
    d.skip(detail_mesh_count * SIZEOF_POLY_DETAIL)?;
    d.skip(detail_vert_count * 12)?; // f32[3]
    d.skip(detail_tri_count * 4)?; // u8[4]
    d.skip(bv_node_count * SIZEOF_BVNODE)?;
    d.skip(off_mesh_con_count * SIZEOF_OFFMESH_CON)?;

    // Self-check: the sections must sum to the declared tile size.
    let computed = d.o;
    let expected = header_size
        + vert_count * 12
        + poly_count * SIZEOF_POLY
        + max_link_count * SIZEOF_LINK
        + detail_mesh_count * SIZEOF_POLY_DETAIL
        + detail_vert_count * 12
        + detail_tri_count * 4
        + bv_node_count * SIZEOF_BVNODE
        + off_mesh_con_count * SIZEOF_OFFMESH_CON;
    if computed != expected || computed != data_size {
        bail!(
            "mmtile: section size mismatch (walked {computed}, computed {expected}, declared \
             {data_size}) — a struct layout is wrong (e.g. 32- vs 64-bit dtPolyRef)"
        );
    }

    Ok(MeshTile {
        x,
        y,
        bmin,
        bmax,
        verts,
        polys,
    })
}
