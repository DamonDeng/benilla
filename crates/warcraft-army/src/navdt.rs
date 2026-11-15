//! Navmesh path finding backed by the vendored Detour (via `navshim.cpp`): load vmangos's `mmaps/`
//! for a map and turn a `MoveTo(x,y,z)` into a list of walkable WoW waypoints. Detour does the
//! cross-tile routing and string-pulling, using the exact mesh the server's creatures path on, so
//! the route is walkable by construction (good for the trusted adapter + anti-cheat).
//!
//! Files (standard vmangos): `<map:03>.mmap` is a `dtNavMeshParams`; `<map:03><y:02><x:02>.mmtile`
//! is a 20-byte `MmapTileHeader` (magic `MMAP`, version 6) then `size` bytes of raw `dtMeshTile`.
//! We strip the header and hand the raw tile to Detour's `addTile` (which reads its own grid coords
//! from the data). All tiles for the map are loaded once, on open.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::os::raw::{c_float, c_int, c_uchar, c_ushort};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};

#[allow(non_camel_case_types)]
type NavHandle = c_void;

extern "C" {
    fn nav_create(params: *const c_uchar, size: c_int) -> *mut NavHandle;
    fn nav_add_tile(h: *mut NavHandle, data: *const c_uchar, size: c_int) -> c_int;
    fn nav_destroy(h: *mut NavHandle);
    fn nav_find_path(
        h: *mut NavHandle,
        sx: c_float, sy: c_float, sz: c_float,
        ex: c_float, ey: c_float, ez: c_float,
        include_flags: c_ushort,
        out_xyz: *mut c_float,
        max_points: c_int,
    ) -> c_int;
    fn nav_height(h: *mut NavHandle, x: c_float, y: c_float, z: c_float, include_flags: c_ushort) -> c_float;
}

/// NavTerrain bits (vmangos MoveMapSharedDefines.h). A normal walking/swimming unit's filter.
const NAV_GROUND: u16 = 0x01;
const NAV_WATER: u16 = 0x08;
const WALK_FILTER: u16 = NAV_GROUND | NAV_WATER;

const MMAP_MAGIC: u32 = u32::from_be_bytes(*b"MMAP");
const MMAP_VERSION: u32 = 6;
/// `MmapTileHeader` is 5 × u32.
const TILE_HEADER_SIZE: usize = 20;
/// Cap on returned waypoints (matches vmangos MAX_POINT_PATH_LENGTH).
const MAX_WAYPOINTS: usize = 256;

/// The server's `mmaps/` directory, from `ARMY_MMAPS_DIR` or a default that assumes a
/// `vmangos-deploy` checkout beside this workspace (relative to the working directory, which is the
/// workspace root for `cargo run`). Set `ARMY_MMAPS_DIR` for any other layout.
pub fn default_mmaps_dir() -> PathBuf {
    std::env::var_os("ARMY_MMAPS_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("../vmangos-deploy/storage/mangosd/extracted-data/mmaps")
    })
}

/// A loaded Detour navmesh for one map. Owns the C handle; freed on drop.
pub struct NavMesh {
    map: u32,
    handle: *mut NavHandle,
    tiles_loaded: usize,
}

// The handle is only touched through `&self`/`&mut self` here and never shared across threads
// without external synchronization; a worker owns its own NavMesh.
unsafe impl Send for NavMesh {}

impl NavMesh {
    /// Load `map`'s navmesh from `dir`: read the `.mmap` params, then add every `.mmtile` for the
    /// map. Errors if the params file is missing/short; individual bad tiles are skipped (logged).
    pub fn open(dir: impl AsRef<Path>, map: u32) -> Result<Self> {
        let dir = dir.as_ref();
        let params_path = dir.join(format!("{map:03}.mmap"));
        let params = fs::read(&params_path).with_context(|| format!("reading {params_path:?}"))?;

        let handle = unsafe { nav_create(params.as_ptr(), params.len() as c_int) };
        if handle.is_null() {
            bail!("Detour: failed to init navmesh for map {map} from {params_path:?}");
        }
        let mut nav = NavMesh { map, handle, tiles_loaded: 0 };

        let prefix = format!("{map:03}");
        for entry in fs::read_dir(dir).with_context(|| format!("listing {dir:?}"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&prefix) || !name.ends_with(".mmtile") {
                continue;
            }
            match nav.add_tile_file(&entry.path()) {
                Ok(()) => nav.tiles_loaded += 1,
                Err(e) => eprintln!("navmesh: skipping {:?}: {e:#}", entry.path()),
            }
        }
        if nav.tiles_loaded == 0 {
            bail!("Detour: no tiles loaded for map {map} (dir {dir:?})");
        }
        eprintln!("navmesh: map {map}: loaded {} tiles", nav.tiles_loaded);
        Ok(nav)
    }

    fn add_tile_file(&mut self, path: &Path) -> Result<()> {
        let bytes = fs::read(path)?;
        if bytes.len() < TILE_HEADER_SIZE {
            bail!("tile too small ({} bytes)", bytes.len());
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != MMAP_MAGIC {
            bail!("bad mmap magic {magic:#x}");
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != MMAP_VERSION {
            bail!("mmap version {version}, expected {MMAP_VERSION}");
        }
        let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let data = bytes
            .get(TILE_HEADER_SIZE..TILE_HEADER_SIZE + size)
            .context("declared tile size exceeds the file")?;
        let rc = unsafe { nav_add_tile(self.handle, data.as_ptr(), data.len() as c_int) };
        if rc != 0 {
            bail!("Detour addTile failed (rc {rc})");
        }
        Ok(())
    }

    /// The map this mesh is for.
    pub fn map(&self) -> u32 {
        self.map
    }

    /// The precise navmesh SURFACE height at WoW `(x,y)` near `z` (the walkable poly under the point:
    /// terrain, a floor, a stair tread), or `None` if there's no walkable poly near it. Used to place
    /// the body on the actual walkable surface each tick.
    pub fn height(&self, p: [f32; 3]) -> Option<f32> {
        let z = unsafe { nav_height(self.handle, p[0], p[1], p[2], WALK_FILTER) };
        if z < -50_000.0 {
            None
        } else {
            Some(z)
        }
    }

    /// Route from `start` to `end` (WoW coords), returning walkable waypoints `[start-ish, …, end-ish]`
    /// snapped onto the mesh, or `None` if either endpoint is off the mesh or no path connects them.
    pub fn find_path(&self, start: [f32; 3], end: [f32; 3]) -> Option<Vec<[f32; 3]>> {
        let mut out = vec![0f32; 3 * MAX_WAYPOINTS];
        let n = unsafe {
            nav_find_path(
                self.handle,
                start[0], start[1], start[2],
                end[0], end[1], end[2],
                WALK_FILTER,
                out.as_mut_ptr(),
                MAX_WAYPOINTS as c_int,
            )
        };
        if n <= 0 {
            return None;
        }
        let n = n as usize;
        Some((0..n).map(|i| [out[i * 3], out[i * 3 + 1], out[i * 3 + 2]]).collect())
    }
}

impl Drop for NavMesh {
    fn drop(&mut self) {
        unsafe { nav_destroy(self.handle) };
    }
}

/// Process-wide cache of one loaded navmesh per map, so all bot sessions SHARE a single mesh (the
/// data is read-only; ~hundreds of MB per continent, not duplicated per bot). The returned handle
/// is `Arc<Mutex<NavMesh>>`: queries serialize behind the mutex, which is fine because a path query
/// is rare (once per MoveTo) and sub-millisecond. A per-thread query pool (for high query throughput)
/// and lazy per-tile loading remain noted optimizations. A map that fails to load is cached as
/// `None` so we don't retry (and thrash) on every call.
#[allow(clippy::type_complexity)]
static REGISTRY: OnceLock<Mutex<HashMap<u32, Option<Arc<Mutex<NavMesh>>>>>> = OnceLock::new();

/// The shared navmesh for `map`, loading it (from the default mmaps dir) on first use. `None` if the
/// map has no usable mmaps.
pub fn shared(map: u32) -> Option<Arc<Mutex<NavMesh>>> {
    let reg = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = reg.lock().unwrap();
    if let Some(entry) = g.get(&map) {
        return entry.clone();
    }
    let dir = default_mmaps_dir();
    let loaded = match NavMesh::open(&dir, map) {
        Ok(n) => Some(Arc::new(Mutex::new(n))),
        Err(e) => {
            eprintln!("navmesh: map {map} unavailable: {e:#}");
            None
        }
    };
    g.insert(map, loaded.clone());
    loaded
}
