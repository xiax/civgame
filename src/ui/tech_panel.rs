use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::faction::{FactionRegistry, PlayerFaction};
use crate::simulation::technology::{ActivityKind, Era, TECH_TREE};

#[derive(Resource, Default)]
pub struct TechPanelOpen(pub bool);

fn activity_name(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Foraging => "Foraging",
        ActivityKind::Farming => "Farming",
        ActivityKind::WoodGathering => "Wood Gathering",
        ActivityKind::StoneMining => "Stone Mining",
        ActivityKind::CoalMining => "Coal Mining",
        ActivityKind::IronMining => "Iron Mining",
        ActivityKind::Combat => "Combat",
        ActivityKind::Socializing => "Socializing",
        ActivityKind::Trading => "Trading",
    }
}

pub fn tech_panel_system(
    mut contexts: EguiContexts,
    open: Res<TechPanelOpen>,
    player_faction: Res<PlayerFaction>,
    registry: Res<FactionRegistry>,
) {
    if !open.0 {
        return;
    }

    let faction_techs = registry
        .factions
        .get(&player_faction.faction_id)
        .map(|d| &d.techs);

    let ctx = contexts.ctx_mut();
    egui::Window::new("Technology Tree")
        .default_pos([620.0, 60.0])
        .default_width(320.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            if faction_techs.is_none() {
                ui.label(egui::RichText::new("No faction yet.").color(egui::Color32::GRAY));
                return;
            }
            let techs = faction_techs.unwrap();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for era in [
                    Era::Paleolithic,
                    Era::Mesolithic,
                    Era::Neolithic,
                    Era::Chalcolithic,
                    Era::BronzeAge,
                ] {
                    egui::CollapsingHeader::new(
                        egui::RichText::new(era.name())
                            .strong()
                            .color(egui::Color32::from_rgb(200, 180, 120))
                            .size(14.0),
                    )
                    .default_open(true)
                    .show(ui, |ui| {
                        for tech in TECH_TREE.iter().filter(|t| t.era == era) {
                            let unlocked = techs.has(tech.id);
                            let prereqs_met =
                                tech.prerequisites.iter().all(|&p| techs.has(p));

                            let (icon, name_color) = if unlocked {
                                ("✓", egui::Color32::from_rgb(80, 210, 100))
                            } else if prereqs_met {
                                ("◎", egui::Color32::from_rgb(240, 200, 60))
                            } else {
                                ("○", egui::Color32::from_gray(100))
                            };

                            let row = ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(icon).color(name_color).size(13.0),
                                );
                                ui.label(
                                    egui::RichText::new(tech.name)
                                        .color(name_color)
                                        .size(13.0),
                                );
                            });

                            let hover_response = ui.interact(
                                row.response.rect,
                                egui::Id::new("tech_hover").with(tech.id),
                                egui::Sense::hover(),
                            );
                            hover_response.on_hover_ui(|ui| {
                                ui.set_max_width(280.0);

                                let status = if unlocked {
                                    ("Unlocked", egui::Color32::from_rgb(80, 210, 100))
                                } else if prereqs_met {
                                    ("Discoverable", egui::Color32::from_rgb(240, 200, 60))
                                } else {
                                    ("Locked", egui::Color32::from_gray(150))
                                };
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(tech.name)
                                            .strong()
                                            .size(14.0)
                                            .color(egui::Color32::WHITE),
                                    );
                                    ui.label(
                                        egui::RichText::new(status.0)
                                            .size(12.0)
                                            .color(status.1),
                                    );
                                });

                                ui.separator();
                                ui.label(
                                    egui::RichText::new(tech.description)
                                        .color(egui::Color32::from_gray(210))
                                        .size(12.0),
                                );

                                if !tech.prerequisites.is_empty() {
                                    ui.add_space(4.0);
                                    let prereq_names: Vec<&str> = tech
                                        .prerequisites
                                        .iter()
                                        .map(|&pid| TECH_TREE[pid as usize].name)
                                        .collect();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Requires: {}",
                                            prereq_names.join(", ")
                                        ))
                                        .color(egui::Color32::from_rgb(180, 140, 80))
                                        .size(11.0),
                                    );
                                }

                                if !tech.triggers.is_empty() {
                                    ui.add_space(2.0);
                                    let trigger_str: Vec<String> = tech
                                        .triggers
                                        .iter()
                                        .map(|t| {
                                            format!(
                                                "{} (+{:.1}%/event)",
                                                activity_name(t.activity),
                                                t.per_unit_chance * 100.0
                                            )
                                        })
                                        .collect();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Discovered via: {}",
                                            trigger_str.join(", ")
                                        ))
                                        .color(egui::Color32::from_rgb(120, 160, 220))
                                        .size(11.0),
                                    );
                                }

                                let b = &tech.bonus;
                                let mut bonus_parts: Vec<String> = Vec::new();
                                if b.food_yield_bonus != 0.0 {
                                    bonus_parts.push(format!(
                                        "+{:.0}% food yield",
                                        b.food_yield_bonus * 100.0
                                    ));
                                }
                                if b.wood_yield_bonus != 0.0 {
                                    bonus_parts.push(format!(
                                        "+{:.0}% wood yield",
                                        b.wood_yield_bonus * 100.0
                                    ));
                                }
                                if b.stone_yield_bonus != 0.0 {
                                    bonus_parts.push(format!(
                                        "+{:.0}% stone yield",
                                        b.stone_yield_bonus * 100.0
                                    ));
                                }
                                if b.food_storage_bonus != 0.0 {
                                    bonus_parts.push(format!(
                                        "+{:.0} food storage",
                                        b.food_storage_bonus
                                    ));
                                }
                                if b.combat_damage_bonus != 0 {
                                    bonus_parts.push(format!("+{} dmg", b.combat_damage_bonus));
                                }
                                if !bonus_parts.is_empty() {
                                    ui.add_space(2.0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Bonus: {}",
                                            bonus_parts.join(", ")
                                        ))
                                        .color(egui::Color32::from_rgb(160, 220, 160))
                                        .size(11.0),
                                    );
                                }
                            });
                        }
                    });

                    ui.add_space(4.0);
                }
            });
        });
}
