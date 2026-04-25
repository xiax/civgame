use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::simulation::combat::{Health, Body, BodyPart};
use crate::simulation::needs::Needs;
use crate::simulation::mood::Mood;
use crate::simulation::skills::{Skills, SkillKind, SKILL_COUNT};
use crate::simulation::person::{PersonAI, AiState, PlayerOrder};
use crate::simulation::goals::{AgentGoal, Personality, GoalReason};
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction, SOLO};
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::jobs::JobKind;
use crate::simulation::plants::{PlantMap, Plant};
use crate::simulation::plan::{ActivePlan, KnownPlans, PlanRegistry, StepRegistry, build_state_vec};
use crate::simulation::neural::UtilityNet;
use crate::simulation::memory::AgentMemory;
use crate::simulation::construction::AutonomousBuildingToggle;
use crate::world::chunk::ChunkMap;
use crate::world::tile::TileKind;
use crate::world::seasons::Calendar;
use crate::economy::agent::EconomicAgent;

use super::selection::SelectedEntity;

pub fn inspector_panel_system(
    mut contexts: EguiContexts,
    selected: Res<SelectedEntity>,
    registry: Res<FactionRegistry>,
    player_faction: Res<PlayerFaction>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    plan_registry: Res<PlanRegistry>,
    step_registry: Res<StepRegistry>,
    calendar: Res<Calendar>,
    plants: Query<&Plant>,
    query: Query<(
        (&Needs, &Mood, &Skills, &PersonAI, &EconomicAgent, &AgentGoal, &Personality, &BiologicalSex, &FactionMember),
        (Option<&Health>, Option<&Body>, Option<&PlayerOrder>, &Transform, Option<&ActivePlan>, Option<&KnownPlans>, Option<&UtilityNet>, Option<&AgentMemory>, Option<&GoalReason>)
    )>,
) {
    let Some(entity) = selected.0 else { return };
    let Ok((
        (needs, mood, skills, ai, agent, goal, personality, sex, member),
        (health, body, order, transform, active_plan, known_plans, utility_net, memory, goal_reason)
    )) = query.get(entity) else { return };

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
            ui.horizontal(|ui| {
                ui.label(format!("Goal: {}", goal.name()));
                if let Some(reason) = goal_reason {
                    ui.label(egui::RichText::new(format!(" ({})", reason.0)).small().color(egui::Color32::from_gray(160)));
                }
            });

            if member.faction_id == SOLO {
                ui.label("Faction: Solo");
                if member.bond_timer > 0 {
                    ui.label(format!("Bonding: {}/180", member.bond_timer));
                }
            } else {
                let food_stock = registry.food_stock(member.faction_id);
                let mut raid_info = if registry.is_under_raid(member.faction_id) {
                    " [UNDER RAID]".to_string()
                } else if let Some(target) = registry.raid_target(member.faction_id) {
                    format!(" [RAIDING #{}]", target)
                } else {
                    String::new()
                };
                if member.faction_id == player_faction.faction_id {
                    raid_info += " [YOU]";
                }
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

            let job_name = match ai.job_id {
                j if j == JobKind::Idle    as u16 => "Idle",
                j if j == JobKind::Gather  as u16 => "Gatherer",
                j if j == JobKind::Trader  as u16 => "Trader",
                j if j == JobKind::Raid    as u16 => "Raider",
                j if j == JobKind::Defend  as u16 => "Defender",
                j if j == JobKind::Planter as u16 => "Planter",
                j if j == JobKind::Hunter  as u16 => "Hunter",
                j if j == JobKind::Scavenge as u16 => "Scavenger",
                j if j == JobKind::ReturnCamp as u16 => "Returning to Camp",
                j if j == JobKind::Socialize as u16 => "Socializing",
                j if j == JobKind::Reproduce as u16 => "Reproducing",
                _ => "Unemployed",
            };
            ui.label(format!("Job: {}", job_name));

            let state_desc = match ai.state {
                AiState::Idle => "Idling".to_string(),
                AiState::Routing => "Traveling (Long Range)".to_string(),
                AiState::Seeking => "Walking to Target".to_string(),
                AiState::Sleeping => "Sleeping".to_string(),
                AiState::Attacking => "In Combat".to_string(),
                AiState::Working => {
                    let tx = ai.target_tile.0 as i32;
                    let ty = ai.target_tile.1 as i32;
                    let mut work_str = "Working".to_string();

                    if ai.job_id == JobKind::Gather as u16 {
                        if let Some(&p_entity) = plant_map.0.get(&(tx, ty)) {
                            if let Ok(plant) = plants.get(p_entity) {
                                work_str = format!("Harvesting {:?}", plant.kind);
                            }
                        } else if let Some(tile_kind) = chunk_map.tile_kind_at(tx, ty) {
                            if tile_kind == TileKind::Stone {
                                work_str = "Mining Stone".to_string();
                            }
                        }
                        // Use u32 to avoid u8 overflow during multiplication
                        work_str = format!("{} ({}%)", work_str, (ai.work_progress as u32 * 100) / 30); // 30 is base stone work_ticks
                    } else if ai.job_id == JobKind::Planter as u16 {
                        work_str = format!("Planting Seeds ({}%)", (ai.work_progress as u32 * 100) / 40); // 40 is TICKS_FARMER_PLANT
                    } else if ai.job_id == JobKind::Raid as u16 {
                        work_str = "Stealing Food".to_string();
                    } else if ai.job_id == JobKind::Scavenge as u16 {
                        work_str = "Picking up item".to_string();
                    }

                    work_str
                }
            };
            ui.label(format!("State: {}", state_desc));
            ui.label(format!("Target: {}, {}", ai.target_tile.0, ai.target_tile.1));

            if let Some(o) = order {
                ui.label(
                    egui::RichText::new(format!(
                        "Order: {} \u{2192} ({}, {})",
                        o.order.label(), o.target_tile.0, o.target_tile.1
                    ))
                    .color(egui::Color32::from_rgb(255, 220, 100)),
                );
            }

            ui.separator();
            if let Some(ap) = active_plan {
                if let Some(plan_def) = plan_registry.0.iter().find(|p| p.id == ap.plan_id) {
                    ui.label(egui::RichText::new(format!("Active Plan: {}", plan_def.name)).strong().color(egui::Color32::LIGHT_BLUE));
                    ui.label(format!("  Step: {}/{}", ap.current_step + 1, plan_def.steps.len()));
                    ui.label(format!("  Reward: {:.2}", ap.reward_acc));
                }
            } else {
                ui.label(egui::RichText::new("Active Plan: None").italics().color(egui::Color32::GRAY));
            }

            if let (Some(kp), Some(net)) = (known_plans, utility_net) {
                egui::CollapsingHeader::new("Available Plans").show(ui, |ui| {
                    let state = build_state_vec(needs, agent, skills, member, memory, &calendar);
                    let cur_tx = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
                    let cur_ty = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
                    let camp_pos = registry.home_tile(member.faction_id);

                    for plan_def in &plan_registry.0 {
                        // Viability checks
                        let serving_goal = plan_def.serves_goals.contains(goal);
                        let known = kp.knows(plan_def.id);
                        let tech_unlocked = plan_def.tech_gate.map_or(true, |tid| {
                            registry.factions.get(&member.faction_id).map(|f| f.techs.has(tid)).unwrap_or(false)
                        });
                        let first_step_ready = plan_def.steps.first()
                            .and_then(|&sid| step_registry.0.iter().find(|s| s.id == sid))
                            .and_then(|s| s.preconditions.requires_good)
                            .map_or(true, |(good, qty)| agent.quantity_of(good) >= qty);

                        let mut rejection = None;
                        if !serving_goal { rejection = Some("Wrong Goal"); }
                        else if !known { rejection = Some("Not Learned"); }
                        else if !tech_unlocked { rejection = Some("Tech Locked"); }
                        else if !first_step_ready { rejection = Some("Missing Materials"); }

                        if let Some(reason) = rejection {
                            ui.label(egui::RichText::new(format!("{}: Rejected ({})", plan_def.name, reason)).small().color(egui::Color32::from_gray(120)));
                        } else {
                            let mut score = net.evaluate_plan(state, plan_def.feature_vec);
                            let mut bonus_str = String::new();
                            if plan_def.id == ai.last_plan_id {
                                score += 0.2;
                                bonus_str += " (+0.2 persist)";
                            }

                            let target_tile = plan_def.memory_target_kind.and_then(|k| {
                                memory.and_then(|m| m.best_for_dist_weighted(k, (cur_tx, cur_ty)))
                            });

                            if let Some(target) = target_tile {
                                let dist_agent = (target.0 as i32 - cur_tx).abs() + (target.1 as i32 - cur_ty).abs();
                                let dist_camp = camp_pos.map_or(0, |c| (target.0 as i32 - c.0 as i32).abs() + (target.1 as i32 - c.1 as i32).abs());
                                let penalty = (dist_agent + dist_camp) as f32 * 0.002;
                                score -= penalty;
                                bonus_str += &format!(" (-{:.2} dist)", penalty);
                            } else {
                                score -= 0.5;
                                bonus_str += " (-0.5 no target)";
                            }

                            ui.horizontal(|ui| {
                                ui.label(format!("{}: ", plan_def.name));
                                ui.label(egui::RichText::new(format!("{:.2}", score)).color(egui::Color32::YELLOW));
                                ui.label(egui::RichText::new(bonus_str).small().color(egui::Color32::from_gray(160)));
                            });
                        }
                    }
                });
            }
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
