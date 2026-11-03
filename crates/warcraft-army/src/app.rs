//! The egui app: the classic three-column shell (icon bar → content) with two views.
//!
//!   AllCharacters  — single-content: every account, online first, each with Login/Logout.
//!   AccountDetails — item-detail: a two-level account→character list, with a detail pane.
//!
//! Styling is default egui for now; the WoW-skinned pass (extracted 1.12.1 textures + fonts) is a
//! later step.

use std::collections::HashMap;
use std::time::Duration;

use eframe::egui;

use benilla_formats::CharCreateCatalog;

use crate::manager::{default_accounts, Manager, Status};
use crate::session::{class_name, race_name, NewChar};

/// Races offered in the create form (`ChrRaces.dbc` ids; all eight are playable in 1.12).
const RACES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
/// Fallback class list when the catalog is unavailable (`ChrClasses.dbc` ids; no 6/10 in 1.12).
const CLASSES: [u8; 9] = [1, 2, 3, 4, 5, 7, 8, 9, 11];

/// Which icon-bar view is showing.
#[derive(PartialEq, Eq, Clone, Copy)]
enum View {
    AllCharacters,
    AccountDetails,
}

/// What the Account Details pane is focused on.
#[derive(PartialEq, Eq, Clone)]
enum Selection {
    None,
    Account(String),
    Character(String, u64),
}

pub struct ArmyApp {
    manager: Manager,
    view: View,
    selection: Selection,
    /// Valid race/class/appearance options from the client's `CharCreateCatalog` (`None` if the
    /// game data isn't found — then the form falls back to a bare all-zero look).
    catalog: Option<CharCreateCatalog>,
    /// Which character (guid) each account's Enter-World picker has selected.
    selected_char: HashMap<String, u64>,
    // Create-character form state (applies to the selected account).
    new_name: String,
    new_race: u8,
    new_class: u8,
    new_gender: u8,
    new_skin: u8,
    new_face: u8,
    new_hair_style: u8,
    new_hair_color: u8,
    new_facial_hair: u8,
}

impl ArmyApp {
    pub fn new() -> Self {
        Self {
            manager: Manager::new(default_accounts()),
            view: View::AllCharacters,
            selection: Selection::None,
            catalog: load_catalog(),
            selected_char: HashMap::new(),
            new_name: String::new(),
            new_race: 1,  // Human
            new_class: 1, // Warrior
            new_gender: 0,
            new_skin: 0,
            new_face: 0,
            new_hair_style: 0,
            new_hair_color: 0,
            new_facial_hair: 0,
        }
    }
}

/// Load the character-creation catalog from the player's game data (via the `WoW` link), for the
/// valid race/class combos and per-race/gender appearance ranges. Returns `None` if the data or a
/// table is missing, in which case the create form degrades to a bare all-zero look.
fn load_catalog() -> Option<CharCreateCatalog> {
    let dir = benilla_formats::wow_data()?;
    let mut chain = benilla_formats::open_chain(&dir).ok()?;
    match CharCreateCatalog::load(&mut chain) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("army: character-creation catalog unavailable ({e:#}); look dials limited");
            None
        }
    }
}

impl eframe::App for ArmyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull worker updates; keep repainting while a login is in flight.
        if self.manager.pump() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        self.icon_bar(ctx);
        match self.view {
            View::AllCharacters => self.all_characters(ctx),
            View::AccountDetails => self.account_details(ctx),
        }
    }
}

impl ArmyApp {
    /// Column 1: the left icon bar that switches views.
    fn icon_bar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("icon_bar")
            .exact_width(84.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    icon_button(ui, &mut self.view, View::AllCharacters, "👥", "Characters");
                    ui.add_space(4.0);
                    icon_button(ui, &mut self.view, View::AccountDetails, "📋", "Accounts");
                });
            });
    }

    /// View 1: single-content list of all accounts, online first.
    fn all_characters(&mut self, ctx: &egui::Context) {
        // Accounts sorted: online, then connecting, then the rest — alphabetical within each.
        let mut order: Vec<String> = self.manager.accounts.iter().map(|a| a.user.clone()).collect();
        order.sort_by_key(|u| {
            let rank = match self.manager.status_of(u) {
                Status::Online { .. } => 0,
                Status::Connecting => 1,
                _ => 2,
            };
            (rank, u.clone())
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("All Characters");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for user in order {
                    self.account_row(ui, &user);
                    ui.separator();
                }
            });
        });
    }

    /// One row in the All Characters list: status, name, controls, and (in world) a movement pad.
    fn account_row(&mut self, ui: &mut egui::Ui, user: &str) {
        let status = self.manager.status_of(user).clone();
        let in_world_now = matches!(&status, Status::Online { in_world: Some(_), .. });
        ui.horizontal(|ui| {
            let (dot, color) = status_dot(&status);
            ui.colored_label(color, dot);
            ui.strong(user);

            match &status {
                Status::Online { roster, in_world } => {
                    if let Some(g) = in_world {
                        let name = roster
                            .iter()
                            .find(|c| c.guid == *g)
                            .map(|c| c.name.as_str())
                            .unwrap_or("?");
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 180, 255),
                            format!("in world: {name}"),
                        );
                        if ui.button("⬅ Leave World").clicked() {
                            self.manager.leave(user);
                        }
                    } else if roster.is_empty() {
                        ui.weak("no characters — create one in Accounts");
                    } else {
                        // Character picker + Enter World.
                        let sel = self
                            .selected_char
                            .entry(user.to_string())
                            .or_insert(roster[0].guid);
                        if !roster.iter().any(|c| c.guid == *sel) {
                            *sel = roster[0].guid;
                        }
                        let sel_name = roster
                            .iter()
                            .find(|c| c.guid == *sel)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        egui::ComboBox::from_id_salt(format!("pick_{user}"))
                            .selected_text(sel_name)
                            .show_ui(ui, |ui| {
                                for c in roster {
                                    ui.selectable_value(
                                        sel,
                                        c.guid,
                                        format!(
                                            "{} — {} {} lvl {}",
                                            c.name,
                                            race_name(c.race),
                                            class_name(c.class),
                                            c.level
                                        ),
                                    );
                                }
                            });
                        let guid = *sel;
                        if ui.button("▶ Enter World").clicked() {
                            self.manager.enter(user, guid);
                        }
                    }
                    if ui.button("⏹ Logout").clicked() {
                        self.manager.logout(user);
                    }
                }
                Status::Connecting => {
                    ui.spinner();
                    ui.label("connecting…");
                }
                Status::Error(msg) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, truncate(msg, 60));
                    if ui.button("▶ Login").clicked() {
                        self.manager.login(user);
                    }
                }
                Status::Offline => {
                    ui.weak("offline");
                    if ui.button("▶ Login").clicked() {
                        self.manager.login(user);
                    }
                }
            }
        });
        if in_world_now {
            self.movement_pad(ui, user);
        }
    }

    /// Movement controls for an in-world bot: live position + cardinal nudges + Stop. Each nudge
    /// walks 20 yd from the current position (straight-line). WoW world coords: +X north, +Y west.
    fn movement_pad(&mut self, ui: &mut egui::Ui, user: &str) {
        const STEP: f32 = 20.0;
        let pose = self.manager.pose_of(user);
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            match pose {
                Some((p, moving)) => {
                    let tag = if moving { "walking" } else { "idle" };
                    ui.weak(format!("@ ({:.0}, {:.0}, {:.0}) · {tag}", p[0], p[1], p[2]));
                    if ui.button("N").clicked() {
                        self.manager.move_to(user, [p[0] + STEP, p[1], p[2]]);
                    }
                    if ui.button("S").clicked() {
                        self.manager.move_to(user, [p[0] - STEP, p[1], p[2]]);
                    }
                    if ui.button("W").clicked() {
                        self.manager.move_to(user, [p[0], p[1] + STEP, p[2]]);
                    }
                    if ui.button("E").clicked() {
                        self.manager.move_to(user, [p[0], p[1] - STEP, p[2]]);
                    }
                    ui.separator();
                    if ui.button("➡ Follow").clicked() {
                        self.manager.follow(user);
                    }
                    if ui.button("⏹ Stop").clicked() {
                        self.manager.stop_moving(user);
                    }
                }
                None => {
                    ui.weak("@ (entering…)");
                }
            }
        });
    }

    /// View 2: item-detail — account→character tree on the left, detail on the right.
    fn account_details(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("account_tree")
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("Accounts");
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let users: Vec<String> =
                        self.manager.accounts.iter().map(|a| a.user.clone()).collect();
                    for user in users {
                        self.tree_account(ui, &user);
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.detail_pane(ui);
        });
    }

    /// One account node and its characters (indented), each selectable.
    fn tree_account(&mut self, ui: &mut egui::Ui, user: &str) {
        let status = self.manager.status_of(user).clone();
        let (dot, color) = status_dot(&status);

        let selected = self.selection == Selection::Account(user.to_string());
        let header = ui.horizontal(|ui| {
            ui.colored_label(color, dot);
            ui.selectable_label(selected, egui::RichText::new(user).strong())
        });
        if header.inner.clicked() {
            self.selection = Selection::Account(user.to_string());
        }

        if let Status::Online { roster, .. } = &status {
            for c in roster {
                let sel = self.selection == Selection::Character(user.to_string(), c.guid);
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    if ui
                        .selectable_label(sel, format!("{} ({})", c.name, class_name(c.class)))
                        .clicked()
                    {
                        self.selection = Selection::Character(user.to_string(), c.guid);
                    }
                });
            }
        }
    }

    /// The right-hand detail for whatever is selected in the tree.
    fn detail_pane(&mut self, ui: &mut egui::Ui) {
        match self.selection.clone() {
            Selection::None => {
                ui.weak("Select an account or character on the left.");
            }
            Selection::Account(user) => {
                ui.heading(&user);
                ui.separator();
                if let Some(a) = self.manager.accounts.iter().find(|a| a.user == user) {
                    ui.label(format!("realmlist: {}", a.realmlist));
                }
                let online = match self.manager.status_of(&user) {
                    Status::Online { roster, .. } => {
                        ui.label(format!("status: online — {} character(s)", roster.len()));
                        true
                    }
                    Status::Connecting => {
                        ui.label("status: connecting…");
                        false
                    }
                    Status::Error(msg) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("error: {msg}"));
                        false
                    }
                    Status::Offline => {
                        ui.label("status: offline");
                        false
                    }
                };
                if online {
                    ui.separator();
                    self.create_form(ui, &user);
                } else {
                    ui.weak("(log the account in to create a character)");
                }
            }
            Selection::Character(user, guid) => {
                let found = match self.manager.status_of(&user) {
                    Status::Online { roster, .. } => {
                        roster.iter().find(|c| c.guid == guid).cloned()
                    }
                    _ => None,
                };
                match found {
                    Some(c) => {
                        ui.heading(&c.name);
                        ui.separator();
                        egui::Grid::new("char_sheet").num_columns(2).show(ui, |ui| {
                            ui.label("Account");
                            ui.label(&user);
                            ui.end_row();
                            ui.label("Race");
                            ui.label(race_name(c.race));
                            ui.end_row();
                            ui.label("Class");
                            ui.label(class_name(c.class));
                            ui.end_row();
                            ui.label("Level");
                            ui.label(c.level.to_string());
                            ui.end_row();
                            ui.label("GUID");
                            ui.label(format!("{:#x}", c.guid));
                            ui.end_row();
                        });
                    }
                    None => {
                        ui.weak("That character is no longer available (account offline?).");
                    }
                }
            }
        }
    }
}

impl ArmyApp {
    /// The classes valid for `race` — from the catalog, else the fallback list.
    fn valid_classes(&self, race: u8) -> Vec<u8> {
        let list = match &self.catalog {
            Some(c) => c.classes_for_race(race),
            None => CLASSES.to_vec(),
        };
        if list.is_empty() {
            CLASSES.to_vec()
        } else {
            list
        }
    }

    /// The five appearance-dial counts for (race, gender), 1 (index 0 only) without a catalog.
    fn look_counts(&self, race: u8, gender: u8) -> (u8, u8, u8, u8, u8) {
        match self.catalog.as_ref().and_then(|c| c.ranges(race, gender)) {
            Some(r) => (r.skin, r.face, r.hair_style, r.hair_color, r.facial_hair),
            None => (1, 1, 1, 1, 1),
        }
    }

    /// The create-character form, shown for an online account in the detail pane.
    fn create_form(&mut self, ui: &mut egui::Ui, user: &str) {
        ui.strong("Create a character");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.new_name);
        });

        egui::ComboBox::from_label("Race")
            .selected_text(race_name(self.new_race))
            .show_ui(ui, |ui| {
                for r in RACES {
                    ui.selectable_value(&mut self.new_race, r, race_name(r));
                }
            });

        // Only classes valid for the chosen race (so the server never refuses the combo).
        let classes = self.valid_classes(self.new_race);
        if !classes.contains(&self.new_class) {
            self.new_class = classes[0];
        }
        egui::ComboBox::from_label("Class")
            .selected_text(class_name(self.new_class))
            .show_ui(ui, |ui| {
                for c in &classes {
                    ui.selectable_value(&mut self.new_class, *c, class_name(*c));
                }
            });

        egui::ComboBox::from_label("Gender")
            .selected_text(if self.new_gender == 0 { "Male" } else { "Female" })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.new_gender, 0u8, "Male");
                ui.selectable_value(&mut self.new_gender, 1u8, "Female");
            });

        // Appearance dials, bounded to the valid per-race/gender counts.
        let (skin_n, face_n, hair_n, _color_cap, facial_n) =
            self.look_counts(self.new_race, self.new_gender);
        dial(ui, "Skin", &mut self.new_skin, skin_n);
        dial(ui, "Face", &mut self.new_face, face_n);
        dial(ui, "Hair", &mut self.new_hair_style, hair_n);
        // Hair colour count depends on the chosen hair style (falls back to the dial cap).
        let color_n = self
            .catalog
            .as_ref()
            .map(|c| c.hair_colors(self.new_race, self.new_gender, self.new_hair_style))
            .filter(|&n| n > 0)
            .unwrap_or(_color_cap);
        dial(ui, "Hair Color", &mut self.new_hair_color, color_n);
        dial(ui, "Facial Hair", &mut self.new_facial_hair, facial_n);

        ui.add_space(6.0);
        let name_ok = !self.new_name.trim().is_empty();
        if ui
            .add_enabled(name_ok, egui::Button::new("➕ Create"))
            .clicked()
        {
            let spec = NewChar {
                name: self.new_name.trim().to_string(),
                race: self.new_race,
                class: self.new_class,
                gender: self.new_gender,
                skin: self.new_skin,
                face: self.new_face,
                hair_style: self.new_hair_style,
                hair_color: self.new_hair_color,
                facial_hair: self.new_facial_hair,
            };
            self.manager.create(user, spec);
            self.new_name.clear();
        }

        if let Some(notice) = self.manager.notice_of(user) {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::LIGHT_RED, notice);
        }
    }
}

/// An appearance dial: a dropdown of `label 1 … label N` (value is the 0-based index). Clamps the
/// stored value into range and collapses to a disabled single option when `count <= 1`.
fn dial(ui: &mut egui::Ui, label: &str, value: &mut u8, count: u8) {
    let count = count.max(1);
    if *value >= count {
        *value = 0;
    }
    if count == 1 {
        ui.add_enabled(
            false,
            egui::Button::new(format!("{label}: {label} 1")).frame(false),
        );
        return;
    }
    egui::ComboBox::from_label(label)
        .selected_text(format!("{label} {}", *value + 1))
        .show_ui(ui, |ui| {
            for i in 0..count {
                ui.selectable_value(value, i, format!("{label} {}", i + 1));
            }
        });
}

/// An icon-bar button: a glyph over a caption, selectable into `current`.
fn icon_button(ui: &mut egui::Ui, current: &mut View, value: View, glyph: &str, caption: &str) {
    let selected = *current == value;
    let text = format!("{glyph}\n{caption}");
    if ui
        .add_sized([72.0, 48.0], egui::SelectableLabel::new(selected, text))
        .clicked()
    {
        *current = value;
    }
}

/// The status glyph and colour for a row.
fn status_dot(status: &Status) -> (&'static str, egui::Color32) {
    match status {
        Status::Online { .. } => ("●", egui::Color32::from_rgb(80, 200, 100)),
        Status::Connecting => ("◐", egui::Color32::from_rgb(230, 200, 90)),
        Status::Error(_) => ("●", egui::Color32::from_rgb(220, 90, 90)),
        Status::Offline => ("○", egui::Color32::GRAY),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
