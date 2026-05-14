use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::HotspotFlowFields;
use crate::pathfinding::path_request::{FailureLog, PathDebugFlags};
use crate::pathfinding::worker::PathfindingDiagnostics;
use crate::rendering::path_debug::PathDebugOverlay;
use crate::simulation::construction::RitualState;
use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::simulation::needs::Needs;
use crate::simulation::person::Person;
use crate::simulation::settlement::{SettlementPlans, ZoneOverlayToggle};
use crate::simulation::skills::{Skills, SKILL_COUNT};
use crate::simulation::technology::{ActivityKind, Era, ACTIVITY_COUNT, TECH_COUNT, TECH_TREE};
use crate::simulation::terraform::{count_terraform_sites_for, TerraformMap, TerraformSite};
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
    ActivityKind::CopperMining,
    ActivityKind::TinMining,
    ActivityKind::GoldMining,
    ActivityKind::SilverMining,
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
    "Copper Mining",
    "Tin Mining",
    "Gold Mining",
    "Silver Mining",
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

#[derive(SystemParam)]
pub struct PathPanelParams<'w> {
    pub path_overlay: ResMut<'w, PathDebugOverlay>,
    pub path_flags: ResMut<'w, PathDebugFlags>,
    pub failure_log: ResMut<'w, FailureLog>,
    pub hotspots: Res<'w, HotspotFlowFields>,
    pub diag: Res<'w, PathfindingDiagnostics>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub connectivity: Res<'w, ChunkConnectivity>,
    pub timing: Res<'w, crate::simulation::speed::SimTimingDiagnostics>,
    pub game_speed: Res<'w, crate::simulation::speed::GameSpeed>,
}

#[derive(SystemParam)]
pub struct DecisionPanelParams<'w> {
    pub interrupt_stats: Res<'w, crate::simulation::opportunistic::OpportunisticInterruptStats>,
    pub decision_metrics: Res<'w, crate::simulation::goal_scorers::DecisionMetrics>,
    pub cohort_registry: Res<'w, crate::simulation::cohort::CohortRegistry>,
    pub opportunity_index: Res<'w, crate::simulation::opportunity::OpportunityIndex>,
    pub sim_clock: Res<'w, crate::simulation::SimClock>,
}

pub fn debug_panel_system(
    mut contexts: EguiContexts,
    mut state: ResMut<DebugPanelState>,
    player_faction: Res<PlayerFaction>,
    mut registry: ResMut<FactionRegistry>,
    plans: Res<SettlementPlans>,
    rituals: Res<RitualState>,
    mut overlay: ResMut<ZoneOverlayToggle>,
    mut path: PathPanelParams,
    selected: Res<SelectedEntity>,
    mut agents: Query<(&mut Needs, &mut Skills, &mut EconomicAgent), With<Person>>,
    terraform_map: Res<TerraformMap>,
    terraform_sites: Query<&TerraformSite>,
    decision_panel: DecisionPanelParams,
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
                // Phase D readout: opportunistic-interrupt counter.
                // `total_fired` accumulates over the session;
                // `last_tick` shows the most recent flip's tick so
                // operators can spot whether interrupts are firing.
                let stats = *decision_panel.interrupt_stats;
                let last_ago = decision_panel
                    .sim_clock
                    .tick
                    .saturating_sub(stats.last_tick);
                let last_txt = if stats.total_fired == 0 {
                    "never".to_string()
                } else {
                    format!("{} ticks ago (tick {})", last_ago, stats.last_tick)
                };
                ui.label(
                    egui::RichText::new(format!(
                        "Opportunistic interrupts: {} total · last {}",
                        stats.total_fired, last_txt
                    ))
                    .small()
                    .color(egui::Color32::from_gray(150)),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "Decisions: {} goal evals · {} scorer evals · avg queue {:.2}",
                        decision_panel.decision_metrics.goal_evaluations,
                        decision_panel.decision_metrics.scorer_evaluations,
                        decision_panel.decision_metrics.average_action_queue_len()
                    ))
                    .small()
                    .color(egui::Color32::from_gray(150)),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "LOD: {} full · {} aggregate · {} dormant · {} cohorts · {} opportunities",
                        decision_panel.decision_metrics.lod_full,
                        decision_panel.decision_metrics.lod_aggregate,
                        decision_panel.decision_metrics.lod_dormant,
                        decision_panel.cohort_registry.cohorts.len(),
                        decision_panel.opportunity_index.len()
                    ))
                    .small()
                    .color(egui::Color32::from_gray(150)),
                );
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

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
                    let catalog = crate::economy::core_ids::catalog();
                    let resources: Vec<crate::economy::resource_catalog::ResourceId> =
                        catalog.iter().map(|(id, _)| id).collect();
                    if give_idx >= resources.len() {
                        give_idx = 0;
                    }
                    ui.horizontal(|ui| {
                        let dn = crate::economy::core_ids::display_name;
                        egui::ComboBox::from_id_salt("give_good")
                            .selected_text(dn(resources[give_idx]))
                            .show_ui(ui, |ui| {
                                for (i, &id) in resources.iter().enumerate() {
                                    ui.selectable_value(&mut give_idx, i, dn(id));
                                }
                            });
                        ui.add(egui::DragValue::new(&mut give_qty).range(1..=9999u32).prefix("×"));
                        if ui.button("Give").clicked() {
                            agent.add_resource(resources[give_idx], give_qty);
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
                        let home_label = if faction.caps.home.is_mobile() {
                            "Camp tile"
                        } else {
                            "Home tile"
                        };
                        ui.label(format!(
                            "{}: ({}, {})",
                            home_label, faction.home_tile.0, faction.home_tile.1
                        ));
                        ui.label(format!("Lifestyle: {}", faction.lifestyle.name()));
                        if faction.caps.home.is_mobile() {
                            if faction.last_migration_tick > 0 {
                                ui.label(format!(
                                    "Last migration: tick {}",
                                    faction.last_migration_tick
                                ));
                            }
                            if let Some(target) = faction.pending_migration {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "⇒ Migrating to ({}, {})",
                                        target.0, target.1
                                    ))
                                    .color(egui::Color32::from_rgb(255, 200, 80)),
                                );
                            }
                            if !faction.recent_camps.is_empty() {
                                ui.label(format!(
                                    "Recent camps: {}",
                                    faction.recent_camps.len()
                                ));
                            }
                        } else if faction.collapse_streak > 0 {
                            // P4: surfacing the failing-streak meter so a
                            // settled faction's slide toward collapse is
                            // visible to the player before SwitchArchetype
                            // fires.
                            ui.label(
                                egui::RichText::new(format!(
                                    "⚠ Collapse streak: {} ticks",
                                    faction.collapse_streak
                                ))
                                .color(egui::Color32::from_rgb(255, 150, 50)),
                            );
                        }
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
                    ui.checkbox(&mut path.path_overlay.show_selected_path, "Selected agent path");
                    ui.checkbox(&mut path.path_overlay.show_flow_fields, "Hotspot flow fields");
                    ui.checkbox(&mut path.path_overlay.show_chunk_graph, "Chunk graph");
                    ui.checkbox(
                        &mut path.path_overlay.show_recent_failures,
                        "Recent failures (red)",
                    );
                    ui.checkbox(
                        &mut path.path_overlay.show_connectivity_components,
                        "Connectivity components",
                    );
                    ui.checkbox(
                        &mut path.path_overlay.show_selected_failures,
                        "Selected agent failures",
                    );
                    ui.separator();
                    ui.checkbox(&mut path.path_flags.verbose_logs, "Verbose pathfinding logs");
                    ui.checkbox(&mut path.path_flags.worker_paused, "Pause worker");
                    let hit_pct = if path.hotspots.lookup_count > 0 {
                        (path.hotspots.lookup_hits as f32 / path.hotspots.lookup_count as f32) * 100.0
                    } else {
                        0.0
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "Hotspot fields: {}  •  Graph nodes: {}",
                            path.hotspots.field_count,
                            path.chunk_graph.edges.len(),
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Hotspot lookups: {} ({:.0}% hit)",
                            path.hotspots.lookup_count, hit_pct,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Worker: {} req/tick  •  {} µs  •  queue {}",
                            path.diag.paths_dispatched_per_tick,
                            path.diag.worker_us_per_tick,
                            path.diag.queue_len,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "A*: {} calls  •  {} iters (max single {})",
                            path.diag.astar_calls_per_tick,
                            path.diag.astar_iters_last_tick,
                            path.diag.astar_iters_max_single,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Connectivity: gen {}  •  {} components / {} nodes",
                            path.connectivity.generation,
                            path.connectivity.component_count(),
                            path.connectivity.node_count(),
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Flow-field hits: {}/tick  •  {} total",
                            path.diag.flow_field_hits_per_tick,
                            path.diag.flow_field_hits_total,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Fastpath misses: {}  •  Stale ids: {}  •  Missing follow: {}",
                            path.diag.hotspot_fastpath_misses,
                            path.diag.stale_id_discards,
                            path.diag.missing_follow_on_event,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Failures: unreachable {} (conn {} / A* {})  •  budget {}  •  no-route {}",
                            path.diag.path_failed_unreachable,
                            path.diag.path_failed_unreachable_connectivity,
                            path.diag.path_failed_unreachable_astar,
                            path.diag.path_failed_budget,
                            path.diag.path_failed_no_route,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Cooldown skips: {}  •  Boundary rejects: {}/tick (total {})",
                            path.diag.path_request_skipped_cooldown,
                            path.diag.boundary_rejections_per_tick,
                            path.diag.boundary_rejections_total,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Step-continuity: planner bad steps {}  •  agent z-drift rejects {}",
                            path.diag.path_failed_step_continuity,
                            path.diag.path_drift_rejections_total,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Failure log: {} entries",
                            path.failure_log.recent.len()
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Clear failure log").clicked() {
                            path.failure_log.clear();
                        }
                    });
                });

                ui.add_space(4.0);

                // ── Sim Timing ────────────────────────────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("Sim Timing")
                        .strong()
                        .color(egui::Color32::from_rgb(180, 180, 240)),
                )
                .default_open(false)
                .show(ui, |ui| {
                    let avg_ms = path.timing.avg_tick_us_ema / 1000.0;
                    let worst_ms = path.timing.worst_tick_us_recent as f32 / 1000.0;
                    let budget_ms = path.game_speed.current.budget_ms_per_tick();
                    let over_budget = avg_ms > budget_ms;
                    let avg_color = if over_budget {
                        egui::Color32::from_rgb(220, 80, 80)
                    } else {
                        egui::Color32::GRAY
                    };
                    let budget_label = if budget_ms.is_finite() {
                        format!("{budget_ms:.0} ms")
                    } else {
                        "∞ (paused)".to_string()
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "Speed: {}  •  budget {}",
                            path.game_speed.current.label(),
                            budget_label,
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Fixed ticks/frame: {}",
                            path.timing.fixed_ticks_this_frame
                        ))
                        .color(egui::Color32::GRAY)
                        .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!("Avg tick: {avg_ms:.2} ms"))
                            .color(avg_color)
                            .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!("Worst tick (60): {worst_ms:.2} ms"))
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
