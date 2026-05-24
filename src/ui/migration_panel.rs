//! Informational migration planning panel for player-controlled
//! nomadic factions.
//!
//! Free-form migration: the player Packs the camp via the HUD button,
//! walks members anywhere using regular Move orders, then Pitches the
//! camp when they find a good spot. This panel exists purely as
//! reference info — it does not gate any movement.
//!
//! Sections:
//! - Status (camp state + current migration intent label)
//! - Intent picker (cosmetic for the player; biases AI autopilot's
//!   `pick_migration_target` scoring when other AI nomads survey)
//! - Send Scouts (8 cardinals) — dispatches a member as a scout
//!   that returns candidate sites
//! - Candidate Sites list (display-only; map pins for "good spots")

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::faction::{
    CampState, FactionRegistry, MigrationIntent, MigrationPhase, PackedMigrationAutonomy,
    PlayerFaction,
};
use crate::simulation::player_command::{CommandSender, PlayerCommand};

/// Toggled by the HUD's "Plan Migration" button.
#[derive(Resource, Default)]
pub struct MigrationPanelOpen(pub bool);

pub fn migration_panel_system(
    mut contexts: EguiContexts,
    mut open: ResMut<MigrationPanelOpen>,
    registry: Res<FactionRegistry>,
    player_faction: Res<PlayerFaction>,
    mut sender: CommandSender,
) {
    if !open.0 {
        return;
    }
    let fid = player_faction.faction_id;
    let Some(faction) = registry.factions.get(&fid) else {
        return;
    };
    if !faction.caps.home.is_mobile() {
        return;
    }
    let chief = faction.chief_entity;

    egui::Window::new("Migration")
        .anchor(egui::Align2::RIGHT_TOP, [-12.0, 60.0])
        .default_width(280.0)
        .resizable(true)
        .show(contexts.ctx_mut(), |ui| {
            if ui.button("Close").clicked() {
                open.0 = false;
            }
            ui.separator();

            // ── Status ──────────────────────────────────────
            ui.label(egui::RichText::new("Status").strong());
            ui.label(format!(
                "Camp: {}",
                match faction.camp_state {
                    CampState::Pitched => "Pitched".to_string(),
                    CampState::Packed { since_tick } =>
                        format!("Packed since tick {}", since_tick),
                }
            ));
            ui.label(format!(
                "Phase: {}",
                match &faction.migration_phase {
                    MigrationPhase::Idle => "Idle".to_string(),
                    MigrationPhase::Surveying { .. } => "Surveying (AI)".to_string(),
                    MigrationPhase::PendingCommit { target, .. } =>
                        format!("Pending → ({}, {})", target.0, target.1),
                    MigrationPhase::PackingCamp { target, .. } =>
                        format!("Packing → ({}, {})", target.0, target.1),
                    MigrationPhase::Traveling {
                        target,
                        caravan_tile,
                        ..
                    } => format!(
                        "Traveling ({}, {}) → ({}, {})",
                        caravan_tile.0, caravan_tile.1, target.0, target.1
                    ),
                    MigrationPhase::PitchingCamp { target, .. } =>
                        format!("Pitching → ({}, {})", target.0, target.1),
                }
            ));
            ui.label(format!("Intent: {}", faction.migration_intent.label()));
            ui.label(
                egui::RichText::new("Tip: Pack Camp on the HUD to enter mobile mode. While Packed, workers wait for explicit orders (Hold) unless you flip Autonomy to Forage. Right-click any tile → Pitch Camp Here to settle.")
                    .color(egui::Color32::from_gray(160))
                    .small(),
            );

            // ── Packed autonomy (only meaningful while Packed) ──
            if matches!(faction.camp_state, CampState::Packed { .. }) {
                ui.separator();
                ui.label(egui::RichText::new("Autonomy").strong());
                ui.label(
                    egui::RichText::new(
                        "Hold: workers wait for direct orders. Forage: food/sleep/social/scout autonomy.",
                    )
                    .color(egui::Color32::from_gray(160))
                    .small(),
                );
                ui.horizontal(|ui| {
                    for mode in [PackedMigrationAutonomy::Hold, PackedMigrationAutonomy::Forage] {
                        let mut btn = egui::Button::new(mode.label());
                        if mode == faction.packed_autonomy {
                            btn = btn.fill(egui::Color32::from_rgb(60, 120, 200));
                        }
                        if ui.add(btn).clicked() {
                            if let Some(c) = chief {
                                sender.send(
                                    vec![c],
                                    PlayerCommand::SetPackedAutonomy { mode },
                                );
                            }
                        }
                    }
                });
            }

            ui.separator();
            // ── Intent picker ───────────────────────────────
            ui.label(egui::RichText::new("Intent").strong());
            ui.label(
                egui::RichText::new("Biases AI autopilot scoring; cosmetic for free-form play.")
                    .color(egui::Color32::from_gray(160))
                    .small(),
            );
            for intent in [
                MigrationIntent::FreeRoute,
                MigrationIntent::FollowWater,
                MigrationIntent::FollowHerds,
                MigrationIntent::SeekWinterShelter,
                MigrationIntent::SeekSummerPasture,
                MigrationIntent::AvoidDanger,
            ] {
                let mut btn = egui::Button::new(intent.label());
                if intent == faction.migration_intent {
                    btn = btn.fill(egui::Color32::from_rgb(60, 120, 200));
                }
                if ui.add(btn).clicked() {
                    if let Some(c) = chief {
                        sender.send(
                            vec![c],
                            PlayerCommand::SetMigrationIntent { intent },
                        );
                    }
                }
            }

            ui.separator();
            // ── Scout dispatch ──────────────────────────────
            ui.label(egui::RichText::new("Send Scouts").strong());
            ui.label(
                egui::RichText::new("Scouts walk out and report candidate camp sites.")
                    .color(egui::Color32::from_gray(160))
                    .small(),
            );
            ui.horizontal(|ui| {
                for (label, dir) in [
                    ("N", 0u8),
                    ("NE", 1),
                    ("E", 2),
                    ("SE", 3),
                ] {
                    if ui.button(label).clicked() {
                        if let Some(c) = chief {
                            sender.send(
                                vec![c],
                                PlayerCommand::SendScout {
                                    direction: dir,
                                    range: 60,
                                },
                            );
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                for (label, dir) in [
                    ("S", 4u8),
                    ("SW", 5),
                    ("W", 6),
                    ("NW", 7),
                ] {
                    if ui.button(label).clicked() {
                        if let Some(c) = chief {
                            sender.send(
                                vec![c],
                                PlayerCommand::SendScout {
                                    direction: dir,
                                    range: 60,
                                },
                            );
                        }
                    }
                }
            });

            ui.separator();
            // ── Candidate sites ─────────────────────────────
            ui.label(egui::RichText::new("Candidate Sites").strong());
            if faction.candidate_sites.is_empty() {
                ui.label(
                    egui::RichText::new("(none — send scouts to discover good spots)")
                        .color(egui::Color32::GRAY),
                );
            } else {
                egui::ScrollArea::vertical()
                    .id_source("candidates_scroll")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (i, c) in faction.candidate_sites.iter().enumerate() {
                            let validated = if c.validated { "✓" } else { " " };
                            ui.label(format!(
                                "{validated} #{i} ({}, {})  score {:.0}",
                                c.anchor.0, c.anchor.1, c.score,
                            ));
                            if !c.reasons.is_empty() {
                                let reasons: Vec<&str> =
                                    c.reasons.iter().map(|r| r.label()).collect();
                                ui.label(
                                    egui::RichText::new(format!("    {}", reasons.join(" · ")))
                                        .color(egui::Color32::from_gray(180))
                                        .small(),
                                );
                            }
                        }
                    });
            }
        });
}
