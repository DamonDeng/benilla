//! Army Manager — a native desktop app (egui) that manages a fleet of headless `benilla-protocol`
//! bot sessions on a 1.12.1 server, the groundwork for agentic teammates.
//!
//! Classic three-column layout: a left icon bar switches between an All-Characters list and an
//! Account-Details master-detail view. See `app.rs` for the UI and `manager.rs`/`session.rs` for
//! the session plumbing.

mod app;
mod manager;
// Detour navmesh reader (pure-Rust): parse → locate → A* route. Not yet wired into the Mover
// (that is the final chunk), so allow the as-yet-unused routing API.
#[allow(dead_code)]
mod navmesh;
mod session;
mod terrain;

use eframe::egui;

fn main() -> eframe::Result {
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
