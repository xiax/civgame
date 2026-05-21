//! Freeform 3D vehicle designer (`plans/vehicle-system.md`, Phase 5).
//!
//! An `egui::Window` that lets the player compose a vehicle on the bounded
//! `GRID_MAX_WIDTH × GRID_MAX_DEPTH × GRID_MAX_HEIGHT` cell grid. The grid is
//! edited one Z-slice at a time (a stacked-floor editor); a live preview runs
//! the shared `derive_stats` / `validate_grid` so the player sees mass,
//! stability, height, the material bill, and validation errors before
//! committing. The Queue button emits `PlayerCommand::QueueCustomVehicle`,
//! which the sim registers into `VehicleDesignRegistry` and assembles like a
//! stock template — the UI stays event-only.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
use crate::simulation::faction::PlayerFaction;
use crate::simulation::vehicle::{
    cell_durability, derive_stats, design_bill, validate_grid, DesignError, VehicleCell,
    VehicleData, VehicleDesign, VehicleDesignId, VehicleDesignRegistry, VehicleGrid,
    VehiclePartKind, VehiclePurpose, GRID_MAX_DEPTH, GRID_MAX_HEIGHT, GRID_MAX_WIDTH,
};

/// The parts the designer can place. The reserved tank/siege kinds
/// (`Engine/Track/ArmorPlate/Turret`) are omitted — no `core.ron` part defs
/// ship for them yet (`plans/vehicle-system-tanks.md`).
const PALETTE: [VehiclePartKind; 10] = [
    VehiclePartKind::Frame,
    VehiclePartKind::Deck,
    VehiclePartKind::Wall,
    VehiclePartKind::Axle,
    VehiclePartKind::Wheel,
    VehiclePartKind::Hitch,
    VehiclePartKind::Yoke,
    VehiclePartKind::CargoBay,
    VehiclePartKind::CrewSeat,
    VehiclePartKind::WeaponMount,
];

/// In-progress design state for the vehicle designer window. UI-only.
#[derive(Resource)]
pub struct VehicleDesignerState {
    pub open: bool,
    pub name: String,
    /// Authoritative working body — `derive_stats` / `validate_grid` read it
    /// directly, so the preview never drifts from what gets queued.
    pub grid: VehicleGrid,
    /// Z-slice currently shown in the grid editor (`0..GRID_MAX_HEIGHT`).
    pub layer: i32,
    pub part: VehiclePartKind,
    /// Index into `VehicleData::materials()`.
    pub material_idx: usize,
    pub purpose: VehiclePurpose,
    pub required_animals: u8,
}

impl Default for VehicleDesignerState {
    fn default() -> Self {
        VehicleDesignerState {
            open: false,
            name: "Custom Vehicle".to_string(),
            grid: VehicleGrid::default(),
            layer: 0,
            part: VehiclePartKind::Frame,
            material_idx: 0,
            purpose: VehiclePurpose::Cargo,
            required_animals: 0,
        }
    }
}

fn part_label(kind: VehiclePartKind) -> &'static str {
    match kind {
        VehiclePartKind::Frame => "Frame",
        VehiclePartKind::Deck => "Deck",
        VehiclePartKind::Wall => "Wall",
        VehiclePartKind::Axle => "Axle",
        VehiclePartKind::Wheel => "Wheel",
        VehiclePartKind::Hitch => "Hitch",
        VehiclePartKind::Yoke => "Yoke",
        VehiclePartKind::CargoBay => "Cargo",
        VehiclePartKind::CrewSeat => "Seat",
        VehiclePartKind::WeaponMount => "Weapon",
        VehiclePartKind::Engine => "Engine",
        VehiclePartKind::Track => "Track",
        VehiclePartKind::ArmorPlate => "Armor",
        VehiclePartKind::Turret => "Turret",
    }
}

/// Two-letter cell glyph for the grid editor.
fn part_glyph(kind: VehiclePartKind) -> &'static str {
    match kind {
        VehiclePartKind::Frame => "Fr",
        VehiclePartKind::Deck => "Dk",
        VehiclePartKind::Wall => "Wl",
        VehiclePartKind::Axle => "Ax",
        VehiclePartKind::Wheel => "Wh",
        VehiclePartKind::Hitch => "Hi",
        VehiclePartKind::Yoke => "Yk",
        VehiclePartKind::CargoBay => "Cg",
        VehiclePartKind::CrewSeat => "Cs",
        VehiclePartKind::WeaponMount => "Wm",
        VehiclePartKind::Engine => "En",
        VehiclePartKind::Track => "Tk",
        VehiclePartKind::ArmorPlate => "Ar",
        VehiclePartKind::Turret => "Tu",
    }
}

fn part_color(kind: VehiclePartKind) -> egui::Color32 {
    match kind {
        VehiclePartKind::Frame => egui::Color32::from_rgb(120, 100, 70),
        VehiclePartKind::Deck => egui::Color32::from_rgb(150, 130, 90),
        VehiclePartKind::Wall => egui::Color32::from_rgb(110, 110, 120),
        VehiclePartKind::Axle => egui::Color32::from_rgb(90, 90, 90),
        VehiclePartKind::Wheel => egui::Color32::from_rgb(60, 60, 60),
        VehiclePartKind::Hitch => egui::Color32::from_rgb(150, 110, 60),
        VehiclePartKind::Yoke => egui::Color32::from_rgb(160, 120, 60),
        VehiclePartKind::CargoBay => egui::Color32::from_rgb(90, 130, 90),
        VehiclePartKind::CrewSeat => egui::Color32::from_rgb(90, 110, 160),
        VehiclePartKind::WeaponMount => egui::Color32::from_rgb(160, 80, 80),
        _ => egui::Color32::from_gray(80),
    }
}

fn describe_error(e: &DesignError) -> String {
    match e {
        DesignError::Empty => "Empty — place at least one cell.".to_string(),
        DesignError::OutOfBounds(p) => format!("Cell ({},{},{}) is out of bounds.", p.x, p.y, p.z),
        DesignError::Disconnected => "Body is not one connected piece.".to_string(),
        DesignError::FloatingCell(p) => {
            format!("Cell ({},{},{}) floats — nothing supports it.", p.x, p.y, p.z)
        }
        DesignError::UnsupportedWheel(p) => {
            format!("Wheel ({},{},{}) is not next to an axle.", p.x, p.y, p.z)
        }
        DesignError::NoControlCell => "No crew seat / hitch / yoke — nothing can drive it.".to_string(),
        DesignError::BadHitch => "Not enough hitch/yoke capacity for the draft animals.".to_string(),
        DesignError::OverloadedAxle => "Axles can't carry the chassis — add axles or lighten it.".to_string(),
        DesignError::BlockedCargo(p) => {
            format!("Cargo cell ({},{},{}) is sealed in — can't load it.", p.x, p.y, p.z)
        }
        DesignError::ChariotRule => "A War vehicle needs a crew seat.".to_string(),
    }
}

fn material_name(rid: ResourceId) -> String {
    crate::economy::core_ids::catalog()
        .get(rid)
        .map(|d| d.display_name.clone())
        .unwrap_or_else(|| "?".to_string())
}

pub fn vehicle_designer_system(
    mut contexts: EguiContexts,
    mut state: ResMut<VehicleDesignerState>,
    data: Res<VehicleData>,
    registry: Res<VehicleDesignRegistry>,
    player_faction: Res<PlayerFaction>,
    mut cmd_events: EventWriter<PlayerCommandEvent>,
) {
    if !state.open {
        return;
    }
    let materials = data.materials();
    if materials.is_empty() {
        return;
    }
    state.material_idx = state.material_idx.min(materials.len() - 1);
    state.layer = state.layer.clamp(0, GRID_MAX_HEIGHT - 1);

    let ctx = contexts.ctx_mut();
    let mut open = state.open;
    egui::Window::new("Vehicle Designer")
        .open(&mut open)
        .default_width(380.0)
        .resizable(true)
        .show(ctx, |ui| {
            // ── Identity ────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut state.name);
            });
            ui.horizontal(|ui| {
                ui.label("Purpose:");
                for (p, lbl) in [
                    (VehiclePurpose::Cargo, "Cargo"),
                    (VehiclePurpose::War, "War"),
                    (VehiclePurpose::Transport, "Transport"),
                ] {
                    ui.selectable_value(&mut state.purpose, p, lbl);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Draft animals:");
                ui.add(egui::DragValue::new(&mut state.required_animals).range(0..=4));
            });

            // ── Saved & AI-proposed designs ─────────────────────────────
            // Designs authored by the player faction: previously-queued
            // customs plus the weekly `vehicle_ai_design_proposal_system`
            // proposals. Loading one re-populates the editor for editing.
            let fid = player_faction.faction_id;
            let have_any = registry
                .iter()
                .any(|d| d.author_faction == Some(fid));
            if have_any {
                egui::CollapsingHeader::new(
                    egui::RichText::new("Saved & proposed designs").strong(),
                )
                .default_open(false)
                .show(ui, |ui| {
                    let mut load: Option<VehicleDesign> = None;
                    for d in registry.iter() {
                        if d.author_faction != Some(fid) {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  ({} cells)", d.name, d.grid.cells.len()));
                            if ui.small_button("Load").clicked() {
                                load = Some(d.clone());
                            }
                        });
                    }
                    if let Some(d) = load {
                        state.name = d.name;
                        state.grid = d.grid;
                        state.purpose = d.allowed_purpose;
                        state.required_animals = d.required_animals;
                        state.layer = 0;
                    }
                });
            }

            ui.separator();

            // ── Part palette + material picker ──────────────────────────
            ui.label(egui::RichText::new("Part").strong());
            ui.horizontal_wrapped(|ui| {
                for kind in PALETTE {
                    let selected = state.part == kind;
                    let btn = egui::Button::new(part_label(kind))
                        .fill(if selected {
                            part_color(kind)
                        } else {
                            egui::Color32::from_gray(55)
                        });
                    if ui.add(btn).clicked() {
                        state.part = kind;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Material:");
                let cur = material_name(materials[state.material_idx].resource);
                egui::ComboBox::from_id_salt("vehicle_designer_material")
                    .selected_text(cur)
                    .show_ui(ui, |ui| {
                        for (i, m) in materials.iter().enumerate() {
                            ui.selectable_value(&mut state.material_idx, i, material_name(m.resource));
                        }
                    });
            });

            ui.separator();

            // ── Layer selector ──────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Z-layer:");
                for z in 0..GRID_MAX_HEIGHT {
                    let count = state.grid.cells.iter().filter(|(p, _)| p.z == z).count();
                    let lbl = format!("{z} ({count})");
                    ui.selectable_value(&mut state.layer, z, lbl);
                }
            });
            ui.label(
                egui::RichText::new(
                    "Left-click places the selected part · right-click clears a cell.",
                )
                .small()
                .color(egui::Color32::from_gray(150)),
            );

            // ── Grid editor — one Z-slice ───────────────────────────────
            let layer = state.layer;
            let place_kind = state.part;
            let place_mat = materials[state.material_idx].resource;
            let place_dur = cell_durability(place_mat, &data);
            egui::Grid::new("vehicle_designer_grid")
                .spacing([2.0, 2.0])
                .show(ui, |ui| {
                    for y in 0..GRID_MAX_DEPTH {
                        for x in 0..GRID_MAX_WIDTH {
                            let pos = IVec3::new(x, y, layer);
                            let existing = state.grid.get(pos).copied();
                            let below = state.grid.contains(pos + IVec3::new(0, 0, -1));
                            let (label, fill) = match existing {
                                Some(c) => (part_glyph(c.kind).to_string(), part_color(c.kind)),
                                None if below => (
                                    "·".to_string(),
                                    egui::Color32::from_rgb(45, 55, 45),
                                ),
                                None => ("·".to_string(), egui::Color32::from_gray(35)),
                            };
                            let btn = egui::Button::new(label)
                                .fill(fill)
                                .min_size(egui::vec2(34.0, 30.0));
                            let resp = ui.add(btn);
                            if resp.clicked() {
                                state.grid.cells.retain(|(p, _)| *p != pos);
                                state.grid.cells.push((
                                    pos,
                                    VehicleCell {
                                        kind: place_kind,
                                        material: place_mat,
                                        durability: place_dur,
                                    },
                                ));
                            } else if resp.secondary_clicked() {
                                state.grid.cells.retain(|(p, _)| *p != pos);
                            }
                        }
                        ui.end_row();
                    }
                });

            ui.separator();

            // ── Live preview — stats + validation ───────────────────────
            let stats = derive_stats(&state.grid, &data);
            let validation = validate_grid(&state.grid, state.purpose, state.required_animals, &data);

            egui::CollapsingHeader::new(egui::RichText::new("Stats").strong())
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(format!(
                        "Cells: {}  ·  footprint {} tiles  ·  height {}",
                        state.grid.cells.len(),
                        stats.footprint_area,
                        stats.height_z
                    ));
                    ui.label(format!(
                        "Empty {} kg  ·  max payload {} kg",
                        stats.empty_mass_g / 1000,
                        stats.max_payload_g / 1000
                    ));
                    ui.label(format!(
                        "Speed: road {:.2}  off-road {:.2}",
                        stats.road_speed_cap, stats.offroad_speed_cap
                    ));
                    ui.label(format!(
                        "Stability {:.2}  ·  turn radius {:.1}  ·  track {:.1}",
                        stats.stability, stats.turn_radius, stats.track_width
                    ));
                    let stress = if stats.stress_margin < 0.0 {
                        egui::RichText::new(format!(
                            "Stress margin {:.0} g — OVERLOADED",
                            stats.stress_margin
                        ))
                        .color(egui::Color32::from_rgb(220, 90, 60))
                    } else {
                        egui::RichText::new(format!("Stress margin {:.0} g", stats.stress_margin))
                    };
                    ui.label(stress);
                });

            // Material bill — built from a throwaway design snapshot.
            let preview = VehicleDesign {
                id: VehicleDesignId(0),
                name: state.name.clone(),
                grid: state.grid.clone(),
                allowed_purpose: state.purpose,
                required_animals: state.required_animals,
                tech_gates: Vec::new(),
                author_faction: None,
                revision: 0,
            };
            egui::CollapsingHeader::new(egui::RichText::new("Material bill").strong())
                .default_open(false)
                .show(ui, |ui| {
                    let bill = design_bill(&preview);
                    if bill.is_empty() {
                        ui.label("(nothing to build yet)");
                    }
                    for (rid, qty) in bill {
                        ui.label(format!("  {} ×{qty}", material_name(rid)));
                    }
                });

            ui.separator();
            match &validation {
                Ok(()) => {
                    ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "\u{2713} Valid design");
                }
                Err(errors) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 90, 60),
                        format!("\u{2717} {} problem(s):", errors.len()),
                    );
                    for e in errors {
                        ui.label(
                            egui::RichText::new(format!("  • {}", describe_error(e)))
                                .small()
                                .color(egui::Color32::from_rgb(210, 140, 120)),
                        );
                    }
                }
            }

            // ── Actions ─────────────────────────────────────────────────
            ui.separator();
            ui.horizontal(|ui| {
                let valid = validation.is_ok() && !state.name.trim().is_empty();
                let queue = egui::Button::new("Queue for Assembly")
                    .fill(egui::Color32::from_rgb(70, 120, 70));
                if ui.add_enabled(valid, queue).clicked() {
                    cmd_events.send(PlayerCommandEvent {
                        actors: Vec::new(),
                        command: PlayerCommand::QueueCustomVehicle {
                            name: state.name.trim().to_string(),
                            grid: state.grid.clone(),
                            purpose: state.purpose,
                            required_animals: state.required_animals,
                        },
                    });
                }
                if ui.button("Clear grid").clicked() {
                    state.grid.cells.clear();
                }
            });
        });
    state.open = open;
}
