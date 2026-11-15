//! Army Manager — a native desktop app (egui) that manages a fleet of headless `benilla-protocol`
//! bot sessions on a 1.12.1 server, the groundwork for agentic teammates.
//!
//! Classic three-column layout: a left icon bar switches between an All-Characters list and an
//! Account-Details master-detail view. See `app.rs` for the UI and `manager.rs`/`session.rs` for
//! the session plumbing.

mod app;
mod manager;
// MCP-over-HTTP control adapter (agent-facing): see src/mcp.rs. Spawned from ArmyApp::new().
mod mcp;
// Detour-backed navmesh path finding (src/navshim.cpp + vendor/detour): load vmangos mmaps and route
// a MoveTo into walkable waypoints. Supersedes the earlier pure-Rust `navmesh` (kept as a reference).
#[allow(dead_code)]
mod navdt;
// Earlier from-scratch navmesh reader (parsing verified; routing was intra-tile only). Retained as a
// reference / format cross-check; Detour (`navdt`) is the path engine now.
#[allow(dead_code)]
mod navmesh;
mod session;
mod terrain;

use eframe::egui;

fn main() -> eframe::Result {
    // Isolated path-finding smoke test, no GUI: `warcraft-army pathtest [map sx sy sz ex ey ez]`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("pathtest") {
        pathtest(&args[2..]);
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Army Manager"),
        ..Default::default()
    };
    eframe::run_native(
        "Army Manager",
        options,
        Box::new(|_cc| Ok(Box::new(app::ArmyApp::new()) as Box<dyn eframe::App>)),
    )
}

/// Load a map's Detour navmesh and print a path. Defaults to a short hop near Northshire Abbey
/// (map 0) when no coords are given: `pathtest [map sx sy sz ex ey ez]`.
fn pathtest(rest: &[String]) {
    let f = |i: usize, d: f32| rest.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let map = rest.first().and_then(|s| s.parse().ok()).unwrap_or(0u32);
    let start = [f(1, -8949.0), f(2, -132.0), f(3, 83.53)];
    let end = [f(4, -9030.0), f(5, -160.0), f(6, 88.0)];

    let dir = navdt::default_mmaps_dir();
    eprintln!("pathtest: map {map}, mmaps {dir:?}");
    let nav = match navdt::NavMesh::open(&dir, map) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("pathtest: open failed: {e:#}");
            std::process::exit(1);
        }
    };
    println!("start {start:?} -> end {end:?}");
    match nav.find_path(start, end) {
        Some(wps) => {
            println!("PATH OK: {} waypoints", wps.len());
            for (i, w) in wps.iter().enumerate() {
                println!("  [{i}] ({:.2}, {:.2}, {:.2})", w[0], w[1], w[2]);
            }
        }
        None => println!("NO PATH (endpoint off-mesh or disconnected)"),
    }
}
