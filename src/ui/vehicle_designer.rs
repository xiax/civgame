//! Freeform 3D vehicle designer — V2 workbench.
//!
//! An `egui::Window` that lets the player compose a vehicle on the bounded
//! `GRID_MAX_WIDTH × GRID_MAX_DEPTH × GRID_MAX_HEIGHT` cell grid. A
//! three-column workbench: a part / variant / module palette on the left, a
//! per-Z-slice grid editor in the centre, and a live `derive_stats` /
//! `validate_grid` / `design_bill` preview on the right. Every part, variant,
//! module, material, stat and validation issue carries a hover explanation.
//! The Queue button emits `PlayerCommand::QueueCustomVehicle`, which the sim
//! registers into `VehicleDesignRegistry` and assembles like a stock template
//! — the UI stays event-only. Queue is disabled while the design needs a tech
//! the player faction does not yet know.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::resource_catalog::ResourceId;
use crate::rendering::entity_sprites::{vehicle_sprite_plan, VehicleSpritePlan};
use crate::simulation::faction::{FactionRegistry, FactionTechs, PlayerFaction};
use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
use crate::simulation::vehicle::{
    cell_durability, collect_design_tech_gates, derive_stats, design_bill,
    design_is_siege_capable, validate_grid, DesignError, MaterialProfile,
    VehicleCell, VehicleData, VehicleDesign, VehicleDesignId, VehicleDesignRegistry,
    VehicleGrid, VehicleModuleInstance, VehiclePartKind,
    VehiclePartVariantId, VehiclePurpose, GRID_MAX_DEPTH, GRID_MAX_HEIGHT, GRID_MAX_WIDTH,
};

/// The parts the designer can place in cell mode.
const PALETTE: [VehiclePartKind; 14] = [
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
    VehiclePartKind::Engine,
    VehiclePartKind::Track,
    VehiclePartKind::ArmorPlate,
    VehiclePartKind::Turret,
];

/// What a left-click in the grid does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditMode {
    /// Place / clear one part cell.
    Cell,
    /// Stamp / clear a whole multi-cell weapon module.
    Module,
}

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
    pub mode: EditMode,
    pub part: VehiclePartKind,
    /// Selected behavioural variant for `part` — `None` is the standard part.
    pub variant: Option<VehiclePartVariantId>,
    /// Index into `VehicleData::materials()`.
    pub material_idx: usize,
    /// Index into `VehicleData::modules()` for module mode.
    pub module_idx: usize,
    /// Selected rotation for the module being stamped.
    pub module_rotation: u8,
    pub purpose: VehiclePurpose,
    pub required_animals: u8,
    /// Heading shown in the inset world-sprite preview (`0..4`,
    /// N/E/S/W). Drives the rotation applied to composed cells via the
    /// shared `vehicle_sprite_plan` helper.
    pub preview_heading: u8,
}

impl Default for VehicleDesignerState {
    fn default() -> Self {
        VehicleDesignerState {
            open: false,
            name: "Custom Vehicle".to_string(),
            grid: VehicleGrid::default(),
            layer: 0,
            mode: EditMode::Cell,
            part: VehiclePartKind::Frame,
            variant: None,
            material_idx: 0,
            module_idx: 0,
            module_rotation: 0,
            purpose: VehiclePurpose::Cargo,
            required_animals: 0,
            preview_heading: 0,
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

/// What a part *does* — the gameplay meaning, not just the label.
fn part_help(kind: VehiclePartKind) -> &'static str {
    match kind {
        VehiclePartKind::Frame => {
            "Structural chassis. Spreads load and ties the body together. A non-bottom \
             cell can rest on a frame."
        }
        VehiclePartKind::Deck => "A flat structural floor — a walkable upper surface.",
        VehiclePartKind::Wall => "A structural side panel — protects what's inside.",
        VehiclePartKind::Axle => {
            "Carries the rolling load. Every wheel must sit next to an axle; more / \
             stronger axles raise the load limit."
        }
        VehiclePartKind::Wheel => {
            "Rolling gear. Must be adjacent to an axle. Wheel material traction sets the \
             road and off-road speed caps."
        }
        VehiclePartKind::Hitch => "Harness point for one draft animal.",
        VehiclePartKind::Yoke => "Harness point for two draft animals.",
        VehiclePartKind::CargoBay => {
            "Holds bulk cargo. Must stay reachable from an open side or a deck/frame."
        }
        VehiclePartKind::CrewSeat => {
            "A seat for a driver or gunner. A vehicle needs one to be driven; turrets \
             need crewed seats to gun."
        }
        VehiclePartKind::WeaponMount => {
            "A light weapon platform — crew fight from it. On its own it is not a siege \
             weapon; group several into a ram / ballista module instead."
        }
        VehiclePartKind::Engine => {
            "Powered traction — an engine-driven vehicle needs no draft animals, but the \
             engine power must exceed the loaded body's drag."
        }
        VehiclePartKind::Track => {
            "Continuous track — load-bearing like a broad axle and high off-road grip; \
             no separate axles needed."
        }
        VehiclePartKind::ArmorPlate => {
            "Heavy protective plating — far tougher cells. May cantilever off a frame."
        }
        VehiclePartKind::Turret => {
            "A rotating weapon ring. Crewed turrets fire at enemies; group into a turret \
             module for a single heavy weapon."
        }
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
        VehiclePartKind::Engine => egui::Color32::from_rgb(170, 120, 40),
        VehiclePartKind::Track => egui::Color32::from_rgb(50, 50, 55),
        VehiclePartKind::ArmorPlate => egui::Color32::from_rgb(130, 140, 150),
        VehiclePartKind::Turret => egui::Color32::from_rgb(180, 70, 70),
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
        DesignError::UnderpoweredEngine => {
            "Engine power can't move the loaded body — add engines or lighten it.".to_string()
        }
        DesignError::ModuleOutOfBounds(_) => {
            "A weapon module extends outside the design grid.".to_string()
        }
        DesignError::ModuleOverlap(_) => {
            "A weapon module overlaps another module's cells.".to_string()
        }
        DesignError::UnsupportedModule(_) => {
            "A heavy weapon module needs more support cells directly beneath it.".to_string()
        }
        DesignError::BadModuleFacing(_) => {
            "A forward-facing weapon must sit on the vehicle's front edge (highest Y).".to_string()
        }
    }
}

fn material_name(rid: ResourceId) -> String {
    crate::economy::core_ids::catalog()
        .get(rid)
        .map(|d| d.display_name.clone())
        .unwrap_or_else(|| "?".to_string())
}

/// Why a material matters — density / strength / traction / durability.
fn material_help(m: &MaterialProfile) -> String {
    format!(
        "Density {}%  ·  strength {}  ·  traction {}  ·  durability {}\n\
         Denser material = heavier cells. Stronger = more load support. More traction \
         = faster wheels/tracks. More durable = tougher cells in combat.",
        m.density_pct, m.strength, m.traction, m.durability
    )
}

/// Plain-language explanation of a derived stat.
fn stat_help(stat: &str) -> &'static str {
    match stat {
        "mass" => "Empty mass is the bare body; max payload is the rated cargo it can carry.",
        "speed" => "Tiles per tick on road / off-road. Set by wheel or track traction (or engine power).",
        "stability" => {
            "Track width ÷ centre-of-mass height. High = hard to tip; low = a tall narrow \
             design rolls over on turns and slopes."
        }
        "turn" => "Turn radius — wider wheelbases turn wider; a steering axle tightens it.",
        "stress" => {
            "Support limit minus loaded mass. Negative means the chassis is structurally \
             overloaded and the design is invalid."
        }
        "engine" => "Summed engine power. Must exceed the loaded body's drag to move.",
        "footprint" => "Distinct ground tiles the body occupies. Height is the Z-levels it spans.",
        _ => "",
    }
}

/// Rotate a module footprint offset 90° CCW in XY (Z unchanged).
fn rot_xy(o: IVec3) -> IVec3 {
    IVec3::new(-o.y, o.x, o.z)
}

/// A module footprint rotated `rotation` quarter-turns and re-anchored so its
/// minimum XY corner sits at `(0, 0)`.
fn rotated_module_footprint(base: &[IVec3], rotation: u8) -> Vec<IVec3> {
    let mut cur: Vec<IVec3> = base.to_vec();
    for _ in 0..(rotation % 4) {
        cur = cur.iter().map(|&o| rot_xy(o)).collect();
    }
    let min_x = cur.iter().map(|o| o.x).min().unwrap_or(0);
    let min_y = cur.iter().map(|o| o.y).min().unwrap_or(0);
    cur.iter()
        .map(|o| IVec3::new(o.x - min_x, o.y - min_y, o.z))
        .collect()
}

/// True when the player faction knows every tech in `gates`.
fn techs_met(gates: &[crate::simulation::technology::TechId], techs: FactionTechs) -> bool {
    gates.iter().all(|t| techs.has(*t))
}

#[allow(clippy::too_many_arguments)]
pub fn vehicle_designer_system(
    mut contexts: EguiContexts,
    mut state: ResMut<VehicleDesignerState>,
    data: Res<VehicleData>,
    registry: Res<VehicleDesignRegistry>,
    player_faction: Res<PlayerFaction>,
    factions: Res<FactionRegistry>,
    mut cmd_events: EventWriter<PlayerCommandEvent>,
    // Threaded into `PlayerCommand::DebugSpawnTestVehicle.spawn_near` so
    // the dispatcher spawns the test vehicle on open ground in front of
    // the player rather than inside the (always-crowded) home tile.
    camera_q: Query<&Transform, With<Camera>>,
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
    if !data.modules().is_empty() {
        state.module_idx = state.module_idx.min(data.modules().len() - 1);
    }
    // A variant is part-specific — drop it if it no longer matches the part.
    if let Some(v) = state.variant {
        if data.variant(v).map(|d| d.part_kind) != Some(state.part) {
            state.variant = None;
        }
    }

    // Player faction tech surface — drives the locked badges + Queue gate.
    let player_techs = factions
        .factions
        .get(&player_faction.faction_id)
        .map(|f| f.techs)
        .unwrap_or_default();

    let ctx = contexts.ctx_mut();
    let mut open = state.open;
    egui::Window::new("Vehicle Designer")
        .open(&mut open)
        .default_width(760.0)
        .resizable(true)
        .show(ctx, |ui| {
            // ── Identity ────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut state.name);
                ui.separator();
                ui.label("Purpose:");
                for (p, lbl) in [
                    (VehiclePurpose::Cargo, "Cargo"),
                    (VehiclePurpose::War, "War"),
                    (VehiclePurpose::Transport, "Transport"),
                ] {
                    ui.selectable_value(&mut state.purpose, p, lbl);
                }
                ui.separator();
                ui.label("Draft animals:")
                    .on_hover_text("Animals the assembled vehicle needs hitched.");
                ui.add(egui::DragValue::new(&mut state.required_animals).range(0..=4));
            });
            ui.separator();

            // ── Three-column workbench ──────────────────────────────────
            ui.columns(3, |cols| {
                draw_palette_column(&mut cols[0], &mut state, &data, materials, player_techs, &registry);
                draw_grid_column(&mut cols[1], &mut state, &data, materials);
                draw_preview_column(&mut cols[2], &mut state, &data, player_techs);
            });

            // ── Actions ─────────────────────────────────────────────────
            ui.separator();
            let validation = validate_grid(
                &state.grid,
                state.purpose,
                state.required_animals,
                &data,
            );
            let gates = collect_design_tech_gates(&state.grid, std::iter::empty(), &data);
            let tech_ok = techs_met(&gates, player_techs);
            ui.horizontal(|ui| {
                let valid = validation.is_ok()
                    && tech_ok
                    && !state.name.trim().is_empty()
                    && !state.grid.cells.is_empty();
                let queue = egui::Button::new("Queue for Assembly")
                    .fill(egui::Color32::from_rgb(70, 120, 70));
                let resp = ui.add_enabled(valid, queue);
                let resp = if !tech_ok {
                    resp.on_disabled_hover_text(
                        "This design uses parts your faction has not yet researched.",
                    )
                } else {
                    resp
                };
                if resp.clicked() {
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
                    state.grid.modules.clear();
                }
                // Save-to-disk: persist the design as a RON template in
                // `assets/data/vehicles/user_<slug>.ron`. The loader picks
                // it up on next game start (it walks every `*.ron` in the
                // dir), so the design survives across runs and can be
                // shared by copying the file.
                let save_valid = validation.is_ok()
                    && !state.name.trim().is_empty()
                    && !state.grid.cells.is_empty();
                let save_btn = egui::Button::new("Save to disk")
                    .fill(egui::Color32::from_rgb(70, 90, 120));
                let save_resp = ui.add_enabled(save_valid, save_btn).on_hover_text(
                    "Write this design to assets/data/vehicles/user_<slug>.ron \
                     so it loads automatically next time the game starts. \
                     Files with the same slug overwrite.",
                );
                if save_resp.clicked() {
                    let preview = crate::simulation::vehicle::VehicleDesign {
                        id: crate::simulation::vehicle::VehicleDesignId(0),
                        name: state.name.trim().to_string(),
                        grid: state.grid.clone(),
                        allowed_purpose: state.purpose,
                        required_animals: state.required_animals,
                        tech_gates: collect_design_tech_gates(
                            &state.grid,
                            std::iter::empty(),
                            &data,
                        ),
                        author_faction: None,
                        from_user_file: false,
                        revision: 0,
                    };
                    match crate::simulation::vehicle::save_custom_design(&preview, &data) {
                        Ok(path) => info!("Saved vehicle design to {:?}", path),
                        Err(e) => warn!("Failed to save vehicle design: {}", e),
                    }
                }
                // Debug Test Drive — present only in debug builds. Bypasses
                // tech gates and the resource bill; ghost draft animals are
                // synthesised if `required_animals > 0`. See
                // `~/.claude/plans/evaluate-the-users-xiao1-civgame-plans-v-optimized-squirrel.md`.
                if cfg!(debug_assertions) {
                    let drive_valid = validation.is_ok()
                        && !state.name.trim().is_empty()
                        && !state.grid.cells.is_empty();
                    let btn = egui::Button::new("Test Drive (debug)")
                        .fill(egui::Color32::from_rgb(120, 80, 40));
                    let resp = ui.add_enabled(drive_valid, btn).on_hover_text(
                        "Spawn this design as a permanent player-faction \
                         vehicle for free, near a Vehicle Yard if you own \
                         one. Bypasses tech gates and resource costs. \
                         Manual drive: W=forward, A/D=turn, Q/E=diagonal, \
                         S=stop, Esc=release.",
                    );
                    if resp.clicked() {
                        let spawn_near = camera_q
                            .get_single()
                            .ok()
                            .map(|t| {
                                crate::world::terrain::world_to_tile(
                                    t.translation.truncate(),
                                )
                            })
                            .unwrap_or((0, 0));
                        cmd_events.send(PlayerCommandEvent {
                            actors: Vec::new(),
                            command: PlayerCommand::DebugSpawnTestVehicle {
                                name: state.name.trim().to_string(),
                                grid: state.grid.clone(),
                                purpose: state.purpose,
                                required_animals: state.required_animals,
                                spawn_near,
                            },
                        });
                    }
                }
            });
        });
    state.open = open;
}

/// Left column — part / variant / material / module palette.
fn draw_palette_column(
    ui: &mut egui::Ui,
    state: &mut VehicleDesignerState,
    data: &VehicleData,
    materials: &[MaterialProfile],
    player_techs: FactionTechs,
    registry: &VehicleDesignRegistry,
) {
    // Mode toggle.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Mode").strong());
        ui.selectable_value(&mut state.mode, EditMode::Cell, "Cell")
            .on_hover_text("Place one part cell per click.");
        ui.selectable_value(&mut state.mode, EditMode::Module, "Module")
            .on_hover_text("Stamp a whole multi-cell weapon module per click.");
    });
    ui.separator();

    match state.mode {
        EditMode::Cell => {
            ui.label(egui::RichText::new("Part").strong());
            ui.horizontal_wrapped(|ui| {
                for kind in PALETTE {
                    let selected = state.part == kind;
                    let locked = data
                        .part(kind)
                        .map(|p| !techs_met(&p.tech_gates, player_techs))
                        .unwrap_or(false);
                    let label = if locked {
                        format!("{} 🔒", part_label(kind))
                    } else {
                        part_label(kind).to_string()
                    };
                    let btn = egui::Button::new(label).fill(if selected {
                        part_color(kind)
                    } else {
                        egui::Color32::from_gray(55)
                    });
                    let resp = ui.add(btn).on_hover_text(part_help(kind));
                    if resp.clicked() {
                        state.part = kind;
                        state.variant = None;
                    }
                }
            });

            // Variant picker.
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Variant").strong());
            let cur_variant_label = state
                .variant
                .and_then(|v| data.variant(v))
                .map(|v| v.label.clone())
                .unwrap_or_else(|| "Standard".to_string());
            egui::ComboBox::from_id_salt("vehicle_designer_variant")
                .selected_text(cur_variant_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.variant, None, "Standard")
                        .on_hover_text("The plain part — no behavioural modifiers.");
                    for v in data.variants_for(state.part) {
                        let locked = !techs_met(&v.tech_gates, player_techs);
                        let label = if locked {
                            format!("{} 🔒", v.label)
                        } else {
                            v.label.clone()
                        };
                        ui.selectable_value(&mut state.variant, Some(v.id), label)
                            .on_hover_text(&v.description);
                    }
                });

            material_picker(ui, state, materials);
        }
        EditMode::Module => {
            ui.label(egui::RichText::new("Weapon module").strong());
            if data.modules().is_empty() {
                ui.label("(no modules loaded)");
            }
            for (i, m) in data.modules().iter().enumerate() {
                let locked = !techs_met(&m.tech_gates, player_techs);
                let label = if locked {
                    format!("{} 🔒", m.label)
                } else {
                    m.label.clone()
                };
                let resp = ui.selectable_label(state.module_idx == i, label);
                let resp = resp.on_hover_text(format!(
                    "{}\n\nCrew {}  ·  gunners {}  ·  range {}  ·  damage {}  ·  siege {}",
                    m.description,
                    m.crew_required,
                    m.gunner_required,
                    m.range,
                    m.damage,
                    m.siege_damage
                ));
                if resp.clicked() {
                    state.module_idx = i;
                    state.module_rotation = 0;
                }
            }
            ui.add_space(4.0);
            // Rotation control + footprint preview.
            if let Some(m) = data.modules().get(state.module_idx) {
                ui.horizontal(|ui| {
                    ui.label("Rotation:");
                    let rots = &m.allowed_rotations;
                    let n = rots.len().max(1);
                    if ui.small_button("⟲").clicked() {
                        let idx = rots
                            .iter()
                            .position(|&r| r == state.module_rotation)
                            .unwrap_or(0);
                        state.module_rotation = rots[(idx + 1) % n];
                    }
                    ui.label(format!("{}°", state.module_rotation as u32 * 90));
                });
                draw_footprint_preview(ui, &rotated_module_footprint(&m.footprint, state.module_rotation));
            }
            material_picker(ui, state, materials);
        }
    }

    // ── Saved & AI-proposed designs ─────────────────────────────────────
    draw_saved_designs(ui, state, registry);
}

fn material_picker(ui: &mut egui::Ui, state: &mut VehicleDesignerState, materials: &[MaterialProfile]) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Material").strong());
    let cur = material_name(materials[state.material_idx].resource);
    egui::ComboBox::from_id_salt("vehicle_designer_material")
        .selected_text(cur)
        .show_ui(ui, |ui| {
            for (i, m) in materials.iter().enumerate() {
                ui.selectable_value(&mut state.material_idx, i, material_name(m.resource))
                    .on_hover_text(material_help(m));
            }
        });
}

/// A small XY footprint preview for the selected module + rotation.
fn draw_footprint_preview(ui: &mut egui::Ui, footprint: &[IVec3]) {
    let max_x = footprint.iter().map(|o| o.x).max().unwrap_or(0);
    let max_y = footprint.iter().map(|o| o.y).max().unwrap_or(0);
    ui.label(egui::RichText::new("Footprint").small());
    egui::Grid::new("module_footprint_preview")
        .spacing([2.0, 2.0])
        .show(ui, |ui| {
            for y in (0..=max_y).rev() {
                for x in 0..=max_x {
                    let filled = footprint.iter().any(|o| o.x == x && o.y == y);
                    let (lbl, fill) = if filled {
                        ("■", egui::Color32::from_rgb(160, 90, 90))
                    } else {
                        ("·", egui::Color32::from_gray(40))
                    };
                    ui.add(
                        egui::Button::new(lbl)
                            .fill(fill)
                            .min_size(egui::vec2(16.0, 16.0)),
                    );
                }
                ui.end_row();
            }
        });
}

fn draw_saved_designs(
    ui: &mut egui::Ui,
    state: &mut VehicleDesignerState,
    registry: &VehicleDesignRegistry,
) {
    ui.separator();
    egui::CollapsingHeader::new(egui::RichText::new("Saved & proposed designs").strong())
        .default_open(false)
        .show(ui, |ui| {
            let mut load: Option<VehicleDesign> = None;
            let mut any = false;
            for d in registry.iter() {
                // Show in-process designs (player saves / AI proposals,
                // both author-tagged) AND on-disk `user_*.ron` designs
                // the loader stamped with `from_user_file`. Stock
                // `core.ron` templates have neither and stay hidden.
                if d.author_faction.is_none() && !d.from_user_file {
                    continue;
                }
                any = true;
                ui.horizontal(|ui| {
                    ui.label(format!("{}  ({} cells)", d.name, d.grid.cells.len()));
                    if ui.small_button("Load").clicked() {
                        load = Some(d.clone());
                    }
                });
            }
            if !any {
                ui.label("(none yet)");
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

/// Centre column — Z tabs + the slice grid editor.
fn draw_grid_column(
    ui: &mut egui::Ui,
    state: &mut VehicleDesignerState,
    data: &VehicleData,
    materials: &[MaterialProfile],
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("Z-layer").strong());
        for z in 0..GRID_MAX_HEIGHT {
            let count = state.grid.cells.iter().filter(|(p, _)| p.z == z).count();
            ui.selectable_value(&mut state.layer, z, format!("{z} ({count})"));
        }
    });
    let hint = match state.mode {
        EditMode::Cell => "Left-click places the selected part · right-click clears a cell.",
        EditMode::Module => {
            "Left-click stamps the module (click = its min corner) · right-click a \
             module cell removes the whole module."
        }
    };
    ui.label(
        egui::RichText::new(hint)
            .small()
            .color(egui::Color32::from_gray(150)),
    );

    let layer = state.layer;
    let place_kind = state.part;
    let place_variant = state.variant;
    let place_mat = materials[state.material_idx].resource;
    let place_dur = cell_durability(place_kind, place_mat, place_variant, data);

    egui::Grid::new("vehicle_designer_grid")
        .spacing([2.0, 2.0])
        .show(ui, |ui| {
            // Render top (max y) row first so "forward" reads upward.
            for y in (0..GRID_MAX_DEPTH).rev() {
                for x in 0..GRID_MAX_WIDTH {
                    let pos = IVec3::new(x, y, layer);
                    let existing = state.grid.get(pos).copied();
                    let below = state.grid.contains(pos + IVec3::new(0, 0, -1));
                    let (label, fill) = match existing {
                        Some(c) => {
                            let g = part_glyph(c.kind).to_string();
                            let g = if c.module_id.is_some() {
                                format!("*{g}")
                            } else {
                                g
                            };
                            (g, part_color(c.kind))
                        }
                        None if below => {
                            ("·".to_string(), egui::Color32::from_rgb(45, 55, 45))
                        }
                        None => ("·".to_string(), egui::Color32::from_gray(35)),
                    };
                    let btn = egui::Button::new(label)
                        .fill(fill)
                        .min_size(egui::vec2(26.0, 24.0));
                    let mut resp = ui.add(btn);
                    if let Some(c) = existing {
                        resp = resp.on_hover_ui(|ui| cell_tooltip(ui, &c, pos, data));
                    }
                    handle_grid_click(&resp, state, data, pos, place_kind, place_variant, place_mat, place_dur);
                }
                ui.end_row();
            }
        });
}

/// Resolve a left / right click on grid cell `pos`.
#[allow(clippy::too_many_arguments)]
fn handle_grid_click(
    resp: &egui::Response,
    state: &mut VehicleDesignerState,
    data: &VehicleData,
    pos: IVec3,
    place_kind: VehiclePartKind,
    place_variant: Option<VehiclePartVariantId>,
    place_mat: ResourceId,
    place_dur: u16,
) {
    match state.mode {
        EditMode::Cell => {
            if resp.clicked() {
                // Don't overwrite a module's cell with a plain part.
                if state.grid.get(pos).and_then(|c| c.module_id).is_some() {
                    return;
                }
                state.grid.cells.retain(|(p, _)| *p != pos);
                state.grid.cells.push((
                    pos,
                    VehicleCell {
                        kind: place_kind,
                        material: place_mat,
                        durability: place_dur,
                        variant: place_variant,
                        module_id: None,
                    },
                ));
            } else if resp.secondary_clicked() {
                if let Some(mid) = state.grid.get(pos).and_then(|c| c.module_id) {
                    state.grid.remove_module(mid);
                } else {
                    state.grid.cells.retain(|(p, _)| *p != pos);
                }
            }
        }
        EditMode::Module => {
            if resp.clicked() {
                stamp_module(state, data, pos, place_mat);
            } else if resp.secondary_clicked() {
                if let Some(mid) = state.grid.get(pos).and_then(|c| c.module_id) {
                    state.grid.remove_module(mid);
                }
            }
        }
    }
}

/// Stamp the selected module so its min XY corner lands at `anchor`.
fn stamp_module(
    state: &mut VehicleDesignerState,
    data: &VehicleData,
    anchor: IVec3,
    material: ResourceId,
) {
    let Some(def) = data.modules().get(state.module_idx) else {
        return;
    };
    let footprint = rotated_module_footprint(&def.footprint, state.module_rotation);
    // Resolve target cells; bail if any is out of bounds or already occupied.
    let mut targets: Vec<IVec3> = Vec::with_capacity(footprint.len());
    for off in &footprint {
        let p = IVec3::new(anchor.x + off.x, anchor.y + off.y, state.layer + off.z);
        if p.x < 0
            || p.y < 0
            || p.z < 0
            || p.x >= GRID_MAX_WIDTH
            || p.y >= GRID_MAX_DEPTH
            || p.z >= GRID_MAX_HEIGHT
            || state.grid.contains(p)
        {
            return;
        }
        targets.push(p);
    }
    let module_id = state.grid.next_module_id();
    let dur = cell_durability(def.part_kind, material, None, data);
    for &p in &targets {
        state.grid.cells.push((
            p,
            VehicleCell {
                kind: def.part_kind,
                material,
                durability: dur,
                variant: None,
                module_id: Some(module_id),
            },
        ));
    }
    state.grid.modules.push(VehicleModuleInstance {
        id: module_id,
        def: def.id,
        cells: targets,
        facing: state.module_rotation,
    });
}

/// Rich hover tooltip for an occupied grid cell.
fn cell_tooltip(ui: &mut egui::Ui, c: &VehicleCell, pos: IVec3, data: &VehicleData) {
    ui.label(
        egui::RichText::new(format!("{} ({},{},{})", part_label(c.kind), pos.x, pos.y, pos.z))
            .strong(),
    );
    ui.label(format!("Material: {}", material_name(c.material)));
    if let Some(v) = c.variant.and_then(|v| data.variant(v)) {
        ui.label(format!("Variant: {}", v.label));
        ui.label(egui::RichText::new(&v.description).small());
    } else {
        ui.label("Variant: Standard");
    }
    ui.label(format!("Durability: {}", c.durability));
    if let Some(mid) = c.module_id {
        ui.label(format!("Part of weapon module #{}", mid.0));
    }
    ui.label(egui::RichText::new(part_help(c.kind)).small());
}

/// Right column — stats, summary, bill, validation, tech gate.
fn draw_preview_column(
    ui: &mut egui::Ui,
    state: &mut VehicleDesignerState,
    data: &VehicleData,
    player_techs: FactionTechs,
) {
    let stats = derive_stats(&state.grid, data);
    let validation = validate_grid(&state.grid, state.purpose, state.required_animals, data);

    let preview = VehicleDesign {
        id: VehicleDesignId(0),
        name: state.name.clone(),
        grid: state.grid.clone(),
        allowed_purpose: state.purpose,
        required_animals: state.required_animals,
        tech_gates: Vec::new(),
        author_faction: None,
        from_user_file: false,
        revision: 0,
    };

    egui::CollapsingHeader::new(egui::RichText::new("Stats").strong())
        .default_open(true)
        .show(ui, |ui| {
            ui.label(format!(
                "Cells: {}  ·  footprint {} tiles  ·  height {}",
                state.grid.cells.len(),
                stats.footprint_area,
                stats.height_z
            ))
            .on_hover_text(stat_help("footprint"));
            ui.label(format!(
                "Empty {} kg  ·  max payload {} kg",
                stats.empty_mass_g / 1000,
                stats.max_payload_g / 1000
            ))
            .on_hover_text(stat_help("mass"));
            ui.label(format!(
                "Speed: road {:.2}  off-road {:.2}",
                stats.road_speed_cap, stats.offroad_speed_cap
            ))
            .on_hover_text(stat_help("speed"));
            ui.label(format!("Stability {:.2}", stats.stability))
                .on_hover_text(stat_help("stability"));
            ui.label(format!(
                "Turn radius {:.1}  ·  track {:.1}",
                stats.turn_radius, stats.track_width
            ))
            .on_hover_text(stat_help("turn"));
            if stats.engine_power > 0 {
                ui.label(format!("Engine power {}", stats.engine_power))
                    .on_hover_text(stat_help("engine"));
            }
            let stress = if stats.stress_margin < 0.0 {
                egui::RichText::new(format!(
                    "Stress margin {:.0} g — OVERLOADED",
                    stats.stress_margin
                ))
                .color(egui::Color32::from_rgb(220, 90, 60))
            } else {
                egui::RichText::new(format!("Stress margin {:.0} g", stats.stress_margin))
            };
            ui.label(stress).on_hover_text(stat_help("stress"));
        });

    // "What this vehicle can do" summary.
    egui::CollapsingHeader::new(egui::RichText::new("What this vehicle can do").strong())
        .default_open(true)
        .show(ui, |ui| {
            let crew = state
                .grid
                .cells
                .iter()
                .filter(|(_, c)| c.kind == VehiclePartKind::CrewSeat)
                .count();
            let ranged = state
                .grid
                .modules
                .iter()
                .filter(|m| data.module_def(m.def).map(|d| d.range > 0).unwrap_or(false))
                .count();
            let siege = design_is_siege_capable(&preview, data);
            ui.label(format!("Cargo capacity: {} kg", stats.max_payload_g / 1000));
            ui.label(format!("Crew seats: {crew}"));
            ui.label(if stats.is_engine_driven {
                "Drive: engine-powered".to_string()
            } else {
                format!("Drive: {} draft animal(s)", state.required_animals)
            });
            ui.label(format!("Mounted ranged weapons: {ranged}"));
            ui.label(if siege {
                "Siege: can smash walls"
            } else {
                "Siege: no"
            });
            ui.label(format!("Max height: {} Z-levels", stats.height_z));
            ui.label(if stats.stability < 1.5 {
                egui::RichText::new("Rollover risk: HIGH")
                    .color(egui::Color32::from_rgb(220, 140, 60))
            } else {
                egui::RichText::new("Rollover risk: low")
            });
        });

    egui::CollapsingHeader::new(egui::RichText::new("Material bill").strong())
        .default_open(false)
        .show(ui, |ui| {
            let bill = design_bill(&preview, data);
            if bill.is_empty() {
                ui.label("(nothing to build yet)");
            }
            for (rid, qty) in bill {
                ui.label(format!("  {} ×{qty}", material_name(rid)));
            }
        });

    // Tech gate line.
    let gates = collect_design_tech_gates(&state.grid, std::iter::empty(), data);
    if !gates.is_empty() {
        if techs_met(&gates, player_techs) {
            ui.colored_label(
                egui::Color32::from_rgb(120, 200, 120),
                "\u{2713} Your faction knows every required tech.",
            );
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(220, 140, 60),
                "\u{1f512} Needs tech your faction has not researched.",
            );
        }
    }

    ui.separator();
    // ── Inset world-sprite preview ───────────────────────────────────
    // Paints the same `vehicle_sprite_plan` the spawned `Vehicle` would
    // render with — so a tall composed body in the inset reads exactly
    // like the body Test-Drive (or assembly) will spawn. Heading buttons
    // pick the facing; egui's painter does the work (no Bevy sprite).
    egui::CollapsingHeader::new(egui::RichText::new("Preview").strong())
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Heading:");
                for (h, label) in [(0u8, "N"), (1, "W"), (2, "S"), (3, "E")] {
                    ui.selectable_value(&mut state.preview_heading, h, label)
                        .on_hover_text("Rotate the preview to see the design from that facing.");
                }
            });
            draw_inset_preview(ui, &preview, state.preview_heading);
            if let Some((lo, hi)) = state.grid.bounds() {
                ui.label(
                    egui::RichText::new(format!(
                        "Footprint: {}×{}  ·  Height: {}",
                        hi.x - lo.x + 1,
                        hi.y - lo.y + 1,
                        hi.z - lo.z + 1
                    ))
                    .small(),
                );
            } else {
                ui.label(egui::RichText::new("(empty grid)").small());
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
                )
                .on_hover_text(describe_error(e));
            }
        }
    }
}

/// Paint the current design as colored quads inside a fixed-size egui canvas.
/// Reuses `vehicle_sprite_plan` so the inset never drifts from what
/// `spawn_vehicle_sprites` produces for a real vehicle.
fn draw_inset_preview(ui: &mut egui::Ui, design: &VehicleDesign, heading: u8) {
    use bevy::prelude::Vec3;
    const CANVAS_W: f32 = 200.0;
    const CANVAS_H: f32 = 140.0;
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(CANVAS_W, CANVAS_H),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    // Subtle background so the preview is visually distinct from the
    // surrounding stats panel.
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(28, 28, 32));
    let plan = vehicle_sprite_plan(design, heading);
    // Adaptive scale: fit the design's local extent inside the canvas
    // with some padding. Without this, a Tank-sized design overflows;
    // a single-cell Hut underuses the space.
    let scale = match &plan {
        VehicleSpritePlan::Composed { cells } if !cells.is_empty() => {
            let max_abs_x = cells
                .iter()
                .map(|c| c.local_offset.x.abs())
                .fold(0.0_f32, f32::max);
            let max_abs_y = cells
                .iter()
                .map(|c| c.local_offset.y.abs())
                .fold(0.0_f32, f32::max);
            let extent_x = max_abs_x * 2.0 + cells[0].size.x;
            let extent_y = max_abs_y * 2.0 + cells[0].size.y;
            let pad = 16.0;
            let sx = (CANVAS_W - pad) / extent_x.max(1.0);
            let sy = (CANVAS_H - pad) / extent_y.max(1.0);
            sx.min(sy).clamp(1.0, 6.0)
        }
        _ => 4.0,
    };
    let centre = rect.center();
    match plan {
        VehicleSpritePlan::Stock => {
            // Hand-drawn stock cart isn't easy to render in egui without
            // pulling the texture handle in here. Approximate with the
            // same brown trapezoid + two wheels.
            let body = egui::Rect::from_center_size(
                centre + egui::vec2(0.0, -scale * 2.0),
                egui::vec2(scale * 12.0, scale * 6.0),
            );
            painter.rect_filled(body, 2.0, egui::Color32::from_rgb(160, 110, 60));
            for x in [-scale * 5.0, scale * 5.0] {
                painter.circle_filled(
                    centre + egui::vec2(x, scale * 2.0),
                    scale * 1.5,
                    egui::Color32::from_rgb(28, 24, 24),
                );
            }
            ui.label(
                egui::RichText::new("(stock cart preview)")
                    .small()
                    .color(egui::Color32::from_rgb(160, 160, 160)),
            );
        }
        VehicleSpritePlan::Composed { cells } => {
            // Map each plan cell's entity-local (x, y, z) into the canvas.
            // `+y` in entity-local is screen-up, so we negate y for the
            // canvas. Z is encoded into the local_offset's y already
            // (cells lift upward by Z); use it for paint order only.
            let mut sorted: Vec<_> = cells.iter().collect();
            sorted.sort_by(|a, b| {
                a.local_offset
                    .z
                    .partial_cmp(&b.local_offset.z)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for cell in sorted {
                let Vec3 { x, y, .. } = cell.local_offset;
                let centre_pt = centre + egui::vec2(x * scale, -y * scale);
                let size = egui::vec2(cell.size.x * scale, cell.size.y * scale);
                let r = egui::Rect::from_center_size(centre_pt, size);
                let c = cell.color.to_srgba();
                let rgba = egui::Color32::from_rgba_premultiplied(
                    (c.red * 255.0) as u8,
                    (c.green * 255.0) as u8,
                    (c.blue * 255.0) as u8,
                    255,
                );
                painter.rect_filled(r, 0.0, rgba);
            }
            if cells.is_empty() {
                painter.text(
                    centre,
                    egui::Align2::CENTER_CENTER,
                    "(place parts to preview)",
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(140, 140, 140),
                );
            }
        }
    }
}
