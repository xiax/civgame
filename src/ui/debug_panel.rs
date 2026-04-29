use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::simulation::needs::Needs;
use crate::simulation::person::Person;
use crate::simulation::construction::RitualState;
use crate::simulation::settlement::{SettlementPlans, ZoneOverlayToggle};
use crate::simulation::terraform::{count_terraform_sites_for, TerraformMap, TerraformSite};
use crate::simulation::skills::{Skills, SKILL_COUNT};
use crate::simulation::technology::{ActivityKind, Era, ACTIVITY_COUNT, TECH_COUNT, TECH_TREE};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::flow_field::FlowFieldCache;
use crate::rendering::path_debug::PathDebugOverlay;
use crate::ui::selection::SelectedEntity;

const SKILL_NAMES: [&str; SKILL_COUNT] = [
    "Farming", "Mining", "Building", "Trading", "Combat", "Crafting", "Social", "Medicine",
];

const ACTIVITY_KINDS: [ActivityKind; ACTIVITY_COUNT] = [
    ActivityKind::Foraging,
    ActivityKind::Farming,
    ActivityKind::WoodGathering,
    ActivityKind::StoneMining,
    ActivityKind::CoalMining,
    ActivityKind::IronMining,
    ActivityKind::Combat,
    ActivityKind::Socializing,
    ActivityKind::Trading,
];

const ACTIVITY_NAMES: [&str; ACTIVITY_COUNT] = [
    "Foraging",
    "Farming",
    "Wood Gathering",
    "Stone Mining",
    "Coal Mining",
    "Iron Mining",
    "Combat",
    "Socializing",
    "Trading",
];

#[derive(Resource)]
pub struct DebugPanelState {
    pub open: bool,
    pub give_good_idx: usize,
    pub give_qty: u32,
    pub give_currency: f32,
}

impl Default for DebugPanelState {
    fn default() -> Self {
        Self {
            open: false,
            give_good_idx: 0,
            give_qty: 10,
            give_currency: 100.0,
        }
    }
}

pub fn debug_panel_system(
    mut contexts: EguiContexts,
    mut state: ResMut<DebugPanelState>,
    player_faction: Res<PlayerFaction>,
    mut registry: ResMut<FactionRegistry>,
    plans: Res<SettlementPlans>,
    rituals: Res<RitualState>,
    mut overlay: ResMut<ZoneOverlayToggle>,
    mut path_overlay: ResMut<PathDebugOverlay>,
    flow_cache: Res<FlowFieldCache>,
    chunk_graph: Res<ChunkGraph>,
    selected: Res<SelectedEntity>,
    mut agents: Query<(&mut Needs, &mut Skills, &mut EconomicAgent), With<Person>>,
    terraform_map: Res<TerraformMap>,
    terraform_sites: Query<&TerraformSite>,
) {
    if !state.open {
        return;
    }

    let fid = player_faction.faction_id;

    // Extract mutable give-item state into locals to avoid borrow conflicts across closures.
    // Written back to state after the window renders.
    let mut give_idx = state.give_good_idx;
    let mut give_qty = state.give_qty;
    let mut give_currency = state.give_currency;

    let ctx = contexts.ctx_mut();
    egui::Window::new("Debug")
        .default_pos([300.0, 60.0])
        .default_width(340.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // ── Technology ────────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Technology")
                        .strong()
                        .color(egui::Color32::from_rgb(200, 160, 255)),
                )
                .default_open(true)
                .show(ui, |ui| {
                    if fid == 0 {
                        ui.label(
                            egui::RichText::new("No faction yet.").color(egui::Color32::GRAY),
                        );
                        return;
                    }
                    if let Some(faction) = registry.factions.get_mut(&fid) {
                        ui.horizontal(|ui| {
                            if ui.button("Unlock All").clicked() {
                                for i in 0..TECH_COUNT as u16 {
                                    faction.techs.unlock(i);
                                }
                            }
                            if ui.button("Lock All").clicked() {
                                faction.techs.0 = 0;
                            }
                        });
                        ui.add_space(4.0);
                        for era in [
                            Era::Paleolithic,
                            Era::Mesolithic,
                            Era::Neolithic,
                            Era::Chalcolithic,
                            Era::BronzeAge,
                        ] {
                            egui::CollapsingHeader::new(
                                egui::RichText::new(era.name())
                                    .color(egui::Color32::from_rgb(200, 180, 120)),
                            )
                            .default_open(false)
                            .show(ui, |ui| {
                                for tech in TECH_TREE.iter().filter(|t| t.era == era) {
                                    let unlocked = faction.techs.has(tech.id);
                                    ui.horizontal(|ui| {
                                        let (icon, color) = if unlocked {
                                            ("✓", egui::Color32::from_rgb(80, 210, 100))
                                        } else {
                                            ("○", egui::Color32::from_gray(130))
                                        };
                                        ui.label(
                                            egui::RichText::new(icon).color(color).size(13.0),
                                        );
                                        ui.label(
                                            egui::RichText::new(tech.name)
                                                .color(color)
                                                .size(13.0),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if unlocked {
                                                    if ui.small_button("Lock").clicked() {
                                                        faction.techs.0 &= !(1u64 << tech.id);
                                                    }
                                                } else if ui.small_button("Unlock").clicked() {
                                                    faction.techs.unlock(tech.id);
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        }
                    }
                });

                ui.add_space(4.0);

                // ── Selected Agent ────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Selected Agent")
                        .strong()
                        .color(egui::Color32::from_rgb(100, 200, 255)),
                )
                .default_open(true)
                .show(ui, |ui| {
                    let Some(entity) = selected.0 else {
                        ui.label(
                            egui::RichText::new("No agent selected.").color(egui::Color32::GRAY),
                        );
                        return;
                    };
                    let Ok((mut needs, mut skills, mut agent)) = agents.get_mut(entity) else {
                        ui.label(
                            egui::RichText::new("Selected entity has no agent data.")
                                .color(egui::Color32::GRAY),
                        );
                        return;
                    };

                    // Needs
                    ui.label(egui::RichText::new("Needs").strong());
                    ui.horizontal(|ui| {
                        if ui
                            .button("Fill All")
                            .on_hover_text("Set all to 0 (fully satisfied)")
                            .clicked()
                        {
                            needs.hunger = 0.0;
                            needs.sleep = 0.0;
                            needs.shelter = 0.0;
                            needs.safety = 0.0;
                            needs.social = 0.0;
                            needs.reproduction = 0.0;
                        }
                        if ui
                            .button("Drain All")
                            .on_hover_text("Set all to 255 (critical)")
                            .clicked()
                        {
                            needs.hunger = 255.0;
                            needs.sleep = 255.0;
                            needs.shelter = 255.0;
                            needs.safety = 255.0;
                            needs.social = 255.0;
                            needs.reproduction = 255.0;
                        }
                    });
                    egui::Grid::new("needs_grid")
                        .num_columns(2)
                        .spacing([8.0, 2.0])
                        .show(ui, |ui| {
                            ui.label("Hunger");
                            ui.add(egui::Slider::new(&mut needs.hunger, 0.0..=255.0));
                            ui.end_row();
                            ui.label("Sleep");
                            ui.add(egui::Slider::new(&mut needs.sleep, 0.0..=255.0));
                            ui.end_row();
                            ui.label("Shelter");
                            ui.add(egui::Slider::new(&mut needs.shelter, 0.0..=255.0));
                            ui.end_row();
                            ui.label("Safety");
                            ui.add(egui::Slider::new(&mut needs.safety, 0.0..=255.0));
                            ui.end_row();
                            ui.label("Social");
                            ui.add(egui::Slider::new(&mut needs.social, 0.0..=255.0));
                            ui.end_row();
                            ui.label("Repro");
                            ui.add(egui::Slider::new(&mut needs.reproduction, 0.0..=255.0));
                            ui.end_row();
                        });

                    ui.add_space(4.0);

                    // Skills
                    ui.label(egui::RichText::new("Skills (XP)").strong());
                    egui::Grid::new("skills_grid")
                        .num_columns(2)
                        .spacing([8.0, 2.0])
                        .show(ui, |ui| {
                            for (i, name) in SKILL_NAMES.iter().enumerate() {
                                ui.label(*name);
                                ui.add(egui::Slider::new(&mut skills.0[i], 0..=10000));
                                ui.end_row();
                            }
                        });

                    ui.add_space(4.0);

                    // Give Item
                    ui.label(egui::RichText::new("Give Item").strong());
                    let goods = Good::all();
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("give_good")
                            .selected_text(goods[give_idx].name())
                            .show_ui(ui, |ui| {
                                for (i, good) in goods.iter().enumerate() {
                                    ui.selectable_value(&mut give_idx, i, good.name());
                                }
                            });
                        ui.add(egui::DragValue::new(&mut give_qty).range(1..=9999u32).prefix("×"));
                        if ui.button("Give").clicked() {
                            agent.add_good(goods[give_idx], give_qty);
                        }
                    });

                    ui.add_space(2.0);

                    // Give Currency
                    ui.label(egui::RichText::new("Currency").strong());
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut give_currency)
                                .range(1.0..=99999.0f32)
                                .prefix("$"),
                        );
                        if ui.button("Give Currency").clicked() {
                            agent.currency += give_currency;
                        }
                        ui.label(
                            egui::RichText::new(format!("(have: ${:.1})", agent.currency))
                                .color(egui::Color32::GRAY)
                                .size(11.0),
                        );
                    });
                });

                ui.add_space(4.0);

                // ── Faction Status ────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Faction Status")
                        .strong()
                        .color(egui::Color32::from_rgb(255, 200, 80)),
                )
                .default_open(true)
                .show(ui, |ui| {
                    if fid == 0 {
                        ui.label(
                            egui::RichText::new("No faction yet.").color(egui::Color32::GRAY),
                        );
                        return;
                    }
                    if let Some(faction) = registry.factions.get(&fid) {
                        ui.label(format!("Members: {}", faction.member_count));
                        ui.label(format!("Home tile: ({}, {})", faction.home_tile.0, faction.home_tile.1));
                        if faction.under_raid {
                            ui.label(
                                egui::RichText::new("⚠ Under Raid!").color(egui::Color32::RED),
                            );
                        }
                        if let Some(target) = faction.raid_target {
                            ui.label(
                                egui::RichText::new(format!("Raiding faction #{target}"))
                                    .color(egui::Color32::from_rgb(255, 150, 50)),
                            );
                        }
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Activity Log (tech discovery progress):")
                                .color(egui::Color32::from_gray(180))
                                .size(12.0),
                        );
                        egui::Grid::new("activity_grid")
                            .num_columns(2)
                            .spacing([8.0, 2.0])
                            .show(ui, |ui| {
                                for (kind, name) in
                                    ACTIVITY_KINDS.iter().zip(ACTIVITY_NAMES.iter())
                                {
                                    let count = faction.activity_log.get(*kind);
                                    ui.label(*name);
                                    ui.label(
                                        egui::RichText::new(count.to_string()).color(if count > 0 {
                                            egui::Color32::from_rgb(100, 220, 100)
                                        } else {
                                            egui::Color32::from_gray(120)
                                        }),
                                    );
                                    ui.end_row();
                                }
                            });
                    }
                });

                ui.add_space(4.0);

                // ── Culture ───────────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Culture")
                        .strong()
                        .color(egui::Color32::from_rgb(255, 140, 200)),
                )
                .default_open(true)
                .show(ui, |ui| {
                    if let Some(faction) = registry.factions.get(&fid) {
                        let c = &faction.culture;
                        ui.label(
                            egui::RichText::new(format!("Style: {}", c.style.label()))
                                .color(egui::Color32::from_rgb(255, 220, 180))
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(format!("Seed: {:#010x}", c.seed))
                                .color(egui::Color32::from_gray(140))
                                .size(11.0),
                        );
                        egui::Grid::new("culture_grid")
                            .num_columns(2)
                            .spacing([8.0, 2.0])
                            .show(ui, |ui| {
                                let traits = [
                                    ("Density", c.density),
                                    ("Defensive", c.defensive),
                                    ("Ceremonial", c.ceremonial),
                                    ("Mercantile", c.mercantile),
                                    ("Martial", c.martial),
                                ];
                                for (name, val) in traits {
                                    ui.label(name);
                                    let pct = (val as f32 / 255.0 * 100.0) as u32;
                                    let color = if val >= 180 {
                                        egui::Color32::from_rgb(255, 140, 80)
                                    } else if val >= 120 {
                                        egui::Color32::from_rgb(220, 200, 100)
                                    } else {
                                        egui::Color32::from_gray(140)
                                    };
                                    ui.label(
                                        egui::RichText::new(format!("{val} ({pct}%)")).color(color),
                                    );
                                    ui.end_row();
                                }
                            });
                    } else {
                        ui.label(
                            egui::RichText::new("No faction yet.").color(egui::Color32::GRAY),
                        );
                    }
                });

                ui.add_space(4.0);

                // ── Lineage ───────────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Lineage")
                        .strong()
                        .color(egui::Color32::from_rgb(180, 220, 255)),
                )
                .default_open(false)
                .show(ui, |ui| {
                    if let Some(faction) = registry.factions.get(&fid) {
                        let l = &faction.lineage;
                        ui.label(format!("Founder: {}", l.founder));
                        ui.label(format!("Root: {}", l.root));
                        ui.label(format!("Generation: {}", l.generation));
                    }
                });

                ui.add_space(4.0);

                // ── Pathing Debug ─────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Pathing Debug")
                        .strong()
                        .color(egui::Color32::from_rgb(180, 180, 240)),
                )
                .default_open(false)
                .show(ui, |ui| {
                    ui.checkbox(&mut path_overlay.show_selected_path, "Selected agent path");
                    ui.checkbox(&mut path_overlay.show_flow_fields, "Cached flow fields");
                    ui.checkbox(&mut path_overlay.show_chunk_graph, "Chunk graph");
                    ui.label(
                        egui::RichText::new(format!(
                            "Cached fields: {}  •  Graph nodes: {}",
                            flow_cache.fields.len(),
                            chunk_graph.edges.len(),
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                });

                ui.add_space(4.0);

                // ── Settlement Plan ───────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Settlement Plan")
                        .strong()
                        .color(egui::Color32::from_rgb(160, 220, 160)),
                )
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut overlay.show, "Show zone overlay");
                        ui.checkbox(&mut overlay.all_factions, "All factions");
                    });
                    match plans.0.get(&fid) {
                        Some(plan) if !plan.zones.is_empty() => {
                            ui.label(format!(
                                "Planned at tick {} • {} zones",
                                plan.planned_at_tick,
                                plan.zones.len()
                            ));
                            for z in &plan.zones {
                                ui.label(format!(
                                    "{}: ({},{}) {}×{}  fill {}/{}",
                                    z.kind.label(),
                                    z.rect.x0,
                                    z.rect.y0,
                                    z.rect.w,
                                    z.rect.h,
                                    z.filled,
                                    z.capacity,
                                ));
                            }
                        }
                        _ => {
                            ui.label(
                                egui::RichText::new("(no plan yet — Phase 2 wires the planner)")
                                    .color(egui::Color32::GRAY)
                                    .size(11.0),
                            );
                        }
                    }
                });

                ui.add_space(4.0);

                // ── Terraforming ──────────────────────────────────────────────
                let terraform_count = count_terraform_sites_for(&terraform_map, &terraform_sites, fid);
                ui.label(format!("Pending terraforms: {}", terraform_count));

                ui.add_space(4.0);

                // ── Recent Rituals ────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Recent Rituals")
                        .strong()
                        .color(egui::Color32::from_rgb(255, 180, 220)),
                )
                .default_open(false)
                .show(ui, |ui| {
                    if rituals.recent_events.is_empty() {
                        ui.label(
                            egui::RichText::new("(none yet — fires on season transition)")
                                .color(egui::Color32::GRAY)
                                .size(11.0),
                        );
                    } else {
                        for ev in rituals.recent_events.iter().rev() {
                            let focal_kind = if ev.uses_monument { "Monument" } else { "Shrine" };
                            ui.label(format!(
                                "F#{} {}  {}  ({},{})  {}m × {}p",
                                ev.faction_id,
                                ev.season.name(),
                                focal_kind,
                                ev.focal.0,
                                ev.focal.1,
                                ev.members_affected,
                                ev.pulse,
                            ));
                        }
                    }
                });
            });
        });

    // Write give-item state back after the window closure consumed the locals.
    state.give_good_idx = give_idx;
    state.give_qty = give_qty;
    state.give_currency = give_currency;
}
