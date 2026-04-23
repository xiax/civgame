use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::combat::{Health, Body, BodyPart};
use crate::simulation::needs::Needs;
use crate::simulation::mood::Mood;
use crate::simulation::skills::{Skills, SkillKind, SKILL_COUNT};
use crate::simulation::person::PersonAI;
use crate::simulation::goals::{AgentGoal, Personality};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::reproduction::BiologicalSex;
use crate::economy::agent::EconomicAgent;

use super::selection::SelectedEntity;

pub fn inspector_panel_system(
    mut contexts: EguiContexts,
    selected: Res<SelectedEntity>,
    registry: Res<FactionRegistry>,
    query: Query<(
        &Needs, &Mood, &Skills, &PersonAI, &EconomicAgent,
        &AgentGoal, &Personality, &BiologicalSex, &FactionMember, Option<&Health>, Option<&Body>
    )>,
) {
    let Some(entity) = selected.0 else { return };
    let Ok((needs, mood, skills, ai, agent, goal, personality, sex, member, health, body)) =
        query.get(entity) else { return };

    egui::Window::new("Inspector")
        .default_pos([10.0, 10.0])
        .default_width(240.0)
        .show(contexts.ctx_mut(), |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Mood: {} ({})", mood.label(), mood.0));
                ui.separator();
                ui.label(sex.name());
            });
            ui.label(format!("Personality: {}", personality.name()));
            ui.label(format!("Goal: {}", goal.name()));

            if member.faction_id == SOLO {
                ui.label("Faction: Solo");
                if member.bond_timer > 0 {
                    ui.label(format!("Bonding: {}/180", member.bond_timer));
                }
            } else {
                let food_stock = registry.food_stock(member.faction_id);
                let raid_info = if registry.is_under_raid(member.faction_id) {
                    " [UNDER RAID]".to_string()
                } else if let Some(target) = registry.raid_target(member.faction_id) {
                    format!(" [RAIDING #{}]", target)
                } else {
                    String::new()
                };
                ui.label(format!("Faction: #{} (food: {:.1}){}", member.faction_id, food_stock, raid_info));
            }

            ui.separator();
            if let Some(h) = health {
                ui.horizontal(|ui| {
                    ui.label(format!("{:8}", "Health"));
                    let frac = h.fraction();
                    let color = egui::Color32::from_rgb(
                        (255.0 * (1.0 - frac)) as u8,
                        (255.0 * frac) as u8,
                        30,
                    );
                    ui.add(egui::ProgressBar::new(frac).desired_width(140.0).fill(color));
                    ui.label(format!("{}/{}", h.current, h.max));
                });
            } else if let Some(b) = body {
                ui.horizontal(|ui| {
                    ui.label(format!("{:8}", "Body Health"));
                    let frac = b.fraction();
                    let color = egui::Color32::from_rgb(
                        (255.0 * (1.0 - frac)) as u8,
                        (255.0 * frac) as u8,
                        30,
                    );
                    ui.add(egui::ProgressBar::new(frac).desired_width(140.0).fill(color));
                });
                egui::CollapsingHeader::new("Limbs").show(ui, |ui| {
                    for part in BodyPart::ALL {
                        let limb = b.parts[part as usize];
                        ui.horizontal(|ui| {
                            ui.label(format!("{:10}", format!("{:?}", part)));
                            let frac = limb.current as f32 / limb.max as f32;
                            let color = egui::Color32::from_rgb(
                                (255.0 * (1.0 - frac)) as u8,
                                (255.0 * frac) as u8,
                                30,
                            );
                            ui.add(egui::ProgressBar::new(frac).desired_width(100.0).fill(color));
                            ui.label(format!("{}/{}", limb.current, limb.max));
                        });
                    }
                });
            }

            ui.separator();
            ui.label("Needs:");
            needs_bar(ui, "Hunger",  needs.hunger);
            needs_bar(ui, "Sleep",   needs.sleep);
            needs_bar(ui, "Shelter", needs.shelter);
            needs_bar(ui, "Safety",  needs.safety);
            needs_bar(ui, "Social",  needs.social);
            needs_bar(ui, "Repro",   needs.reproduction);

            ui.separator();
            ui.label("Skills:");
            for i in 0..SKILL_COUNT {
                let kind = unsafe { std::mem::transmute::<u8, SkillKind>(i as u8) };
                ui.label(format!("  {}: {}", kind.name(), skills.0[i]));
            }

            ui.separator();
            ui.label(format!("Currency: {:.1}", agent.currency));
            ui.label("Inventory:");
            for (item, qty) in &agent.inventory {
                if *qty > 0 {
                    let mut name = item.good.name().to_string();
                    if let Some(mat) = item.material {
                        name = format!("{:?} {}", mat, name);
                    }
                    if let Some(qual) = item.quality {
                        name = format!("{} ({:?})", name, qual);
                    }
                    ui.label(format!("  {}: {}", name, qty));
                }
            }

            ui.separator();
            ui.label(format!("State: {:?}", ai.state));
            ui.label(format!(
                "Job: {}",
                if ai.job_id == PersonAI::UNEMPLOYED { "None".to_string() } else { format!("#{}", ai.job_id) }
            ));
        });
}

fn needs_bar(ui: &mut egui::Ui, label: &str, value: f32) {
    ui.horizontal(|ui| {
        ui.label(format!("{label:8}"));
        let progress = value / 255.0;
        let color = egui::Color32::from_rgb(
            (255.0 * progress) as u8,
            (255.0 * (1.0 - progress)) as u8,
            30,
        );
        let bar = egui::ProgressBar::new(progress)
            .desired_width(140.0)
            .fill(color);
        ui.add(bar);
        ui.label(format!("{value}"));
    });
}
