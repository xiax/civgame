use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::mode::EconomicMode;
use crate::rendering::camera::CameraViewZ;
use crate::simulation::combat::CombatTarget;
use crate::simulation::construction::AutonomousBuildingToggle;
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction};
use crate::simulation::faction::HuntOrder;
use crate::simulation::person::{Drafted, Person, Profession};
use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
use crate::simulation::schedule::SimClock;
use crate::simulation::technology::HUNTING_SPEAR;
use crate::ui::debug_panel::DebugPanelState;
use crate::ui::selection::SelectedEntities;
use crate::ui::tech_panel::TechPanelOpen;
use crate::world::seasons::Calendar;

/// Set by the Draft button (or `R` keypress); consumed by
/// `apply_draft_toggle_system` once per frame.
#[derive(Resource, Default)]
pub struct DraftToggleRequest(pub bool);

/// Bundle the HUD's resource handles. Bevy caps a system at 16 top-level
/// params, and the HUD now has more toggles + queries than that, so the
/// resources fold into one SystemParam.
#[derive(SystemParam)]
pub struct HudResources<'w> {
    pub clock: ResMut<'w, SimClock>,
    pub mode: ResMut<'w, EconomicMode>,
    pub auto_build: ResMut<'w, AutonomousBuildingToggle>,
    pub tech_panel_open: ResMut<'w, TechPanelOpen>,
    pub debug_state: ResMut<'w, DebugPanelState>,
    pub draft_req: ResMut<'w, DraftToggleRequest>,
    pub camera_view_z: Res<'w, CameraViewZ>,
    pub calendar: Res<'w, Calendar>,
    pub player_faction: Res<'w, PlayerFaction>,
    pub registry: Res<'w, FactionRegistry>,
    pub selected_many: Res<'w, SelectedEntities>,
    pub catalog: Res<'w, crate::economy::resource_catalog::ResourceCatalog>,
}

pub fn hud_system(
    mut contexts: EguiContexts,
    mut res: HudResources,
    drafted_q: Query<(), With<Drafted>>,
    persons: Query<(), With<Person>>,
    professions: Query<(Entity, &Profession, &FactionMember), With<Person>>,
    mut cmd_events: EventWriter<PlayerCommandEvent>,
) {
    let clock = &mut *res.clock;
    let mode = &mut *res.mode;
    let auto_build = &mut *res.auto_build;
    let tech_panel_open = &mut *res.tech_panel_open;
    let debug_state = &mut *res.debug_state;
    let draft_req = &mut *res.draft_req;
    let camera_view_z = &*res.camera_view_z;
    let calendar = &*res.calendar;
    let player_faction = &*res.player_faction;
    let registry = &*res.registry;
    let selected_many = &*res.selected_many;
    let catalog = &*res.catalog;
    let pop = persons.iter().count();
    let any_drafted = selected_many.ids.iter().any(|&e| drafted_q.get(e).is_ok());
    let has_selection = !selected_many.ids.is_empty();
    let player_hunters: Vec<Entity> = professions
        .iter()
        .filter(|(_, p, m)| {
            **p == Profession::Hunter && m.faction_id == player_faction.faction_id
        })
        .map(|(e, _, _)| e)
        .collect();
    let current_hunters = player_hunters.len();
    let hunting_unlocked = registry
        .factions
        .get(&player_faction.faction_id)
        .map(|f| f.techs.has(HUNTING_SPEAR))
        .unwrap_or(false);

    egui::Area::new(egui::Id::new("hud"))
        .fixed_pos([0.0, 0.0])
        .show(contexts.ctx_mut(), |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Population: {pop}"))
                                .color(egui::Color32::WHITE)
                                .size(16.0),
                        );
                        ui.separator();

                        ui.label(egui::RichText::new("Speed:").color(egui::Color32::WHITE));

                        let active_fill = egui::Color32::from_rgb(60, 120, 200);
                        for (label, target) in
                            [("⏸", 0.0_f32), ("1×", 1.0), ("2×", 2.0), ("5×", 5.0)]
                        {
                            let btn = egui::Button::new(label);
                            let btn = if (clock.speed - target).abs() < 0.01 {
                                btn.fill(active_fill)
                            } else {
                                btn
                            };
                            if ui.add(btn).clicked() {
                                clock.speed = target;
                            }
                        }

                        ui.separator();
                        if ui.button(mode.label()).clicked() {
                            *mode = mode.cycle();
                        }

                        ui.separator();
                        let build_lbl = if auto_build.0 {
                            "Build: ON"
                        } else {
                            "Build: OFF"
                        };
                        let build_btn = egui::Button::new(build_lbl).fill(if auto_build.0 {
                            egui::Color32::from_rgb(60, 180, 80)
                        } else {
                            egui::Color32::from_gray(60)
                        });
                        if ui.add(build_btn).clicked() {
                            auto_build.0 = !auto_build.0;
                        }

                        ui.separator();
                        let tech_btn = egui::Button::new("Tech").fill(if tech_panel_open.0 {
                            egui::Color32::from_rgb(140, 100, 200)
                        } else {
                            egui::Color32::from_gray(60)
                        });
                        if ui.add(tech_btn).clicked() {
                            tech_panel_open.0 = !tech_panel_open.0;
                        }

                        ui.separator();
                        let (draft_lbl, draft_fill) = if any_drafted {
                            ("Undraft (R)", egui::Color32::from_rgb(200, 70, 50))
                        } else {
                            ("Draft (R)", egui::Color32::from_gray(60))
                        };
                        let draft_btn = egui::Button::new(draft_lbl).fill(draft_fill);
                        let resp = ui.add_enabled(has_selection, draft_btn);
                        if resp.clicked() {
                            draft_req.0 = true;
                        }

                        ui.separator();
                        ui.label(egui::RichText::new("Hunters:").color(egui::Color32::WHITE));
                        // Read-only display of the chief-set hunting status.
                        // Player no longer adjusts the count directly; the chief
                        // (faction_hunter_assignment_system) decides headcount,
                        // and chief_hunt_order_system posts the active directive.
                        let order_label = registry
                            .factions
                            .get(&player_faction.faction_id)
                            .and_then(|f| f.hunt_order.as_ref())
                            .map(|o| match o {
                                HuntOrder::Hunt {
                                    species,
                                    target_party_size,
                                    mustered,
                                    deployed_tick,
                                    ..
                                } => {
                                    let prefix = if deployed_tick.is_some() {
                                        "Deployed"
                                    } else {
                                        "Hunting"
                                    };
                                    format!(
                                        "{} {} {}/{}",
                                        prefix,
                                        species.label(),
                                        mustered.len().min(*target_party_size as usize),
                                        target_party_size
                                    )
                                }
                                HuntOrder::Scout { .. } => "Scouting".to_string(),
                            })
                            .unwrap_or_else(|| "Idle".to_string());
                        let display = format!("{} · {}", current_hunters, order_label);
                        ui.label(
                            egui::RichText::new(display)
                                .color(if hunting_unlocked {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::GRAY
                                }),
                        );
                        let muster_btn = egui::Button::new("Muster")
                            .fill(egui::Color32::from_rgb(180, 100, 60));
                        let muster_resp =
                            ui.add_enabled(hunting_unlocked && current_hunters > 0, muster_btn);
                        if muster_resp.clicked() {
                            cmd_events.send(PlayerCommandEvent {
                                actors: player_hunters.clone(),
                                command: PlayerCommand::Muster,
                            });
                        }

                        // Pack Camp / Pitch Camp button — shown only for
                        // mobile (nomadic) player factions. Pack Camp
                        // emits the command on the chief; Pitch Camp is
                        // issued via right-click "Pitch Camp Here" on a
                        // target tile.
                        if let Some(player_fac) =
                            registry.factions.get(&player_faction.faction_id)
                        {
                            if player_fac.caps.home.is_mobile() {
                                ui.separator();
                                let chief = player_fac.chief_entity;
                                use crate::simulation::faction::CampState;
                                let (label, fill, enabled) = match player_fac.camp_state {
                                    CampState::Pitched => (
                                        "Pack Camp",
                                        egui::Color32::from_rgb(160, 120, 60),
                                        chief.is_some(),
                                    ),
                                    CampState::Packed { .. } => (
                                        "Mobile (right-click → Pitch)",
                                        egui::Color32::from_rgb(120, 80, 200),
                                        false,
                                    ),
                                };
                                let camp_btn = egui::Button::new(label).fill(fill);
                                let resp = ui.add_enabled(enabled, camp_btn);
                                if resp.clicked() {
                                    if let Some(c) = chief {
                                        cmd_events.send(PlayerCommandEvent {
                                            actors: vec![c],
                                            command: PlayerCommand::PackCamp,
                                        });
                                    }
                                }
                            }
                        }

                        ui.separator();
                        let dbg_btn = egui::Button::new("Debug").fill(if debug_state.open {
                            egui::Color32::from_rgb(200, 100, 60)
                        } else {
                            egui::Color32::from_gray(60)
                        });
                        if ui.add(dbg_btn).clicked() {
                            debug_state.open = !debug_state.open;
                        }

                        ui.separator();
                        let season_color = match calendar.season {
                            crate::world::seasons::Season::Spring => {
                                egui::Color32::from_rgb(100, 220, 100)
                            }
                            crate::world::seasons::Season::Summer => {
                                egui::Color32::from_rgb(255, 220, 50)
                            }
                            crate::world::seasons::Season::Autumn => {
                                egui::Color32::from_rgb(230, 130, 30)
                            }
                            crate::world::seasons::Season::Winter => {
                                egui::Color32::from_rgb(180, 220, 255)
                            }
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "Year {} · {} Day {}",
                                calendar.year,
                                calendar.season.name(),
                                calendar.day
                            ))
                            .color(season_color),
                        );

                        ui.separator();
                        ui.label(
                            egui::RichText::new(format!("Tick: {}", clock.tick))
                                .color(egui::Color32::GRAY),
                        );

                        ui.separator();
                        let z_label = if camera_view_z.0 == i32::MAX {
                            "Z: Surface".to_string()
                        } else {
                            format!("Z: {} (PgUp/PgDn)", camera_view_z.0)
                        };
                        let z_color = if camera_view_z.0 == i32::MAX {
                            egui::Color32::GRAY
                        } else {
                            egui::Color32::from_rgb(255, 200, 80)
                        };
                        ui.label(egui::RichText::new(z_label).color(z_color));
                    });

                    let fid = player_faction.faction_id;
                    if fid != 0 {
                        if let Some(data) = registry.factions.get(&fid) {
                            ui.horizontal(|ui| {
                                let mut storage_text = format!(
                                    "Your Faction #{fid}:  food: {:.1}  |  members: {}",
                                    data.storage.food_total(),
                                    data.member_count
                                );

                                // Append other non-empty goods
                                let mut other_entries: Vec<_> = data
                                    .storage
                                    .totals
                                    .iter()
                                    .filter(|(_, &qty)| qty > 0)
                                    .collect();
                                
                                if !other_entries.is_empty() {
                                    // Storage is now ResourceId-keyed; pull
                                    // display names from the catalog.
                                    let name_of = |id: &crate::economy::resource_catalog::ResourceId| {
                                        catalog
                                            .get(*id)
                                            .map(|d| d.display_name.as_str())
                                            .unwrap_or("?")
                                    };
                                    other_entries.sort_by(|a, b| name_of(a.0).cmp(name_of(b.0)));
                                    for (id, qty) in other_entries {
                                        storage_text.push_str(&format!(
                                            "  |  {}: {}",
                                            name_of(id),
                                            qty
                                        ));
                                    }
                                }

                                ui.label(
                                    egui::RichText::new(storage_text)
                                        .color(egui::Color32::from_rgb(140, 215, 255)),
                                );
                            });
                        }
                    }
                });
        });
}

/// Translates a Draft/Undraft toggle (HUD button or `R` key) into a
/// `PlayerCommand::Muster` or `Disband` event. UI determines eligibility
/// (player-faction subset of the selection) and which direction to flip
/// (drafting if any are not yet drafted; undrafting only when all already
/// drafted). The dispatcher does the state mutation.
pub fn apply_draft_toggle_system(
    mut req: ResMut<DraftToggleRequest>,
    keys: Res<ButtonInput<KeyCode>>,
    mut contexts: EguiContexts,
    selected_many: Res<SelectedEntities>,
    player_faction: Res<PlayerFaction>,
    drafted_q: Query<(), With<Drafted>>,
    faction_q: Query<&FactionMember>,
    mut cmd_events: EventWriter<PlayerCommandEvent>,
) {
    let mut requested = req.0;
    req.0 = false;

    // `R` keybind, ignored when egui has keyboard focus (typing into a panel).
    let typing = contexts.ctx_mut().wants_keyboard_input();
    if !typing && keys.just_pressed(KeyCode::KeyR) {
        requested = true;
    }
    if !requested || selected_many.ids.is_empty() {
        return;
    }

    let player_only: Vec<Entity> = selected_many
        .ids
        .iter()
        .copied()
        .filter(|e| {
            faction_q
                .get(*e)
                .map(|m| m.faction_id == player_faction.faction_id)
                .unwrap_or(false)
        })
        .collect();
    if player_only.is_empty() {
        return;
    }
    let any_undrafted = player_only.iter().any(|e| drafted_q.get(*e).is_err());
    let command = if any_undrafted {
        PlayerCommand::Muster
    } else {
        PlayerCommand::Disband
    };
    cmd_events.send(PlayerCommandEvent {
        actors: player_only,
        command,
    });
}
