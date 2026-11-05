//! Fetch-on-miss content delivery — Phase B of `docs/online-content-updates.md`.
//!
//! When the terrain streamer meets a map the catalog does not know, [`crate::world_map`]'s
//! `refresh_content` asks this module to fetch it from the **content origin** — a local directory
//! standing in for the server/CDN, named by `BENILLA_CONTENT_ORIGIN`. A per-map JSON manifest lists
//! the files; each is copied into the loose overlay and sha256-verified, and its catalog row is
//! merged on top of `Map.dbc` (no binary `Map.dbc` patching). The copied files persist on disk, so
//! after a restart the first miss simply re-reads the (cheap) manifest and re-derives the row —
//! no separate registry is needed.
//!
//! The origin is a local directory today; swapping it for an HTTPS endpoint later changes only the
//! transport inside [`fetch_map`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The content origin, from `BENILLA_CONTENT_ORIGIN`. `None` disables fetch-on-miss, leaving the
/// client to only re-read already-local content (Phase A).
#[derive(Resource, Default, Clone)]
pub struct ContentOrigin(pub Option<PathBuf>);

impl ContentOrigin {
    pub fn from_env() -> Self {
        match std::env::var_os("BENILLA_CONTENT_ORIGIN") {
            Some(v) if !v.is_empty() => {
                let p = PathBuf::from(v);
                info!("content origin: {}", p.display());
                ContentOrigin(Some(p))
            }
            _ => ContentOrigin(None),
        }
    }
}

/// A map's catalog row fetched at runtime and merged over the shipped `Map.dbc`.
#[derive(Clone)]
pub struct FetchedRow {
    pub map_id: u32,
    pub folder: String,
    pub map_name: String,
    pub instance_type: u32,
    pub loading_screen: u32,
}

/// Maps fetched this session, keyed by id. The on-disk files persist across restarts; this
/// in-memory set is re-derived from the origin on the first miss after a restart.
#[derive(Resource, Default)]
pub struct FetchedMaps(pub HashMap<u32, FetchedRow>);

#[derive(Deserialize)]
struct Manifest {
    map_id: u32,
    folder: String,
    map_name: String,
    #[serde(default)]
    instance_type: u32,
    #[serde(default)]
    loading_screen: u32,
    #[serde(default)]
    version: u32,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
struct ManifestFile {
    /// Path relative to the client's Data overlay root, e.g. `World/Maps/Sandbox2/Sandbox2.wdt`.
    path: String,
    sha256: String,
    #[serde(default)]
    size: u64,
}

/// Fetch map `map_id` from `origin` into the loose `overlay`: read its manifest, copy each listed
/// file (verifying sha256 and rejecting path escapes), and return its catalog row. On any error the
/// map is left uncatalogued (the caller logs and the map stays unknown until the next miss).
pub fn fetch_map(origin: &Path, map_id: u32, overlay: &Path) -> Result<FetchedRow, String> {
    let map_root = origin.join("maps").join(map_id.to_string());
    let manifest_path = map_root.join("manifest.json");
    let bytes = std::fs::read(&manifest_path)
        .map_err(|e| format!("no manifest at {}: {e}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("bad manifest {}: {e}", manifest_path.display()))?;
    if manifest.map_id != map_id {
        return Err(format!("manifest map_id {} != requested {map_id}", manifest.map_id));
    }

    let mut total = 0u64;
    for f in &manifest.files {
        // Keep writes under the overlay: no parent escapes, no absolute paths.
        if f.path.split(['/', '\\']).any(|seg| seg == "..") || f.path.starts_with(['/', '\\']) {
            return Err(format!("unsafe file path in manifest: {}", f.path));
        }
        let src = map_root.join(&f.path);
        let data = std::fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
        let got = hex(Sha256::digest(&data).as_slice());
        if !got.eq_ignore_ascii_case(&f.sha256) {
            return Err(format!("sha256 mismatch for {}: manifest {}, got {got}", f.path, f.sha256));
        }
        let dest = overlay.join(&f.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::write(&dest, &data).map_err(|e| format!("write {}: {e}", dest.display()))?;
        total += f.size.max(data.len() as u64);
    }

    info!(
        "content fetch: map {map_id} v{} ({}) — {} file(s), {total} bytes verified into the overlay",
        manifest.version,
        manifest.folder,
        manifest.files.len()
    );
    Ok(FetchedRow {
        map_id: manifest.map_id,
        folder: manifest.folder,
        map_name: manifest.map_name,
        instance_type: manifest.instance_type,
        loading_screen: manifest.loading_screen,
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
