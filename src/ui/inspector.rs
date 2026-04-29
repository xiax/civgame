use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::simulation::combat::{Body, BodyPart, Health};
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction, SOLO};
use crate::simulation::goals::{AgentGoal, GoalReason, Personality};
use crate::simulation::memory::{AgentMemory, RelEntry, RelationshipMemory};
use crate::simulation::mood::Mood;
use crate::simulation::needs::Needs;
use crate::simulation::neural::UtilityNet;
use crate::simulation::person::{AiState, PersonAI, PlayerOrder, Profession};
use crate::simulation::plan::{
    build_state_vec, ActivePlan, KnownPlans, PlanRegistry, StepRegistry,
};
use crate::simulation::plants::{Plant, PlantMap};
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::skills::{SkillKind, Skills, SKILL_COUNT};
use crate::pathfinding::path_request::{
    FailReason, FailureLog, FollowStatus, PathFollow, PathRequestQueue,
};
use crate::simulation::tasks::TaskKind;
use crate::world::chunk::ChunkMap;
use crate::world::seasons::Calendar;
use crate::world::tile::TileKind;

use super::selection::SelectedEntity;

#[derive(SystemParam)]
pub struct PathInspectorParams<'w, 's> {
    pub failure_log: Res<'w, FailureLog>,
    pub path_queue: ResMut<'w, PathRequestQueue>,
    pub path_follows: Query<'w, 's, &'static mut PathFollow>,
}

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
    mut path_params: PathInspectorParams,
    plants: Query<&Plant>,
    rel_query: Query<&RelationshipMemory>,
    name_query: Query<&Name>,
    query: Query<(
        (
            &Needs,
            &Mood,
            &Skills,
            &PersonAI,
            &EconomicAgent,
            &AgentGoal,
            &Personality,
            &BiologicalSex,
            &FactionMember,
            &Profession,
        ),
        (
            Option<&Health>,
            Option<&Body>,
            Option<&PlayerOrder>,
            &Transform,
            Option<&ActivePlan>,
            Option<&KnownPlans>,
            Option<&UtilityNet>,
            Option<&AgentMemory>,
            Option<&GoalReason>,
        ),
    )>,
) {
    let Some(entity) = selected.0 else { return };
    let Ok((
        (needs, mood, skills, ai, agent, goal, personality, sex, member, profession),
        (
            health,
            body,
            order,
            transform,
            active_plan,
            known_plans,
            utility_net,
            memory,
            goal_reason,
        ),
    )) = query.get(entity)
    else {
        return;
    };
    let rel_mem = rel_query.get(entity).ok();

    egui::Window::new("Inspector")
        .default_pos([10.0, 10.0])
        .default_width(240.0)
        .show(contexts.ctx_mut(), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Mood: {} ({})", mood.label(), mood.0));
                    ui.separator();
                    ui.label(sex.name());
                });
                ui.label(format!("Personality: {}", personality.name()));
                ui.label(format!("Profession: {:?}", profession));
                ui.horizontal(|ui| {
                    ui.label(format!("Goal: {}", goal.name()));
                    if let Some(reason) = goal_reason {
                        ui.label(
                            egui::RichText::new(format!(" ({})", reason.0))
                                .small()
                                .color(egui::Color32::from_gray(160)),
                        );
                    }
                });

                if member.faction_id == SOLO {
                    ui.label("Faction: Solo");
                    if member.bond_timer > 0 {
                        ui.label(format!("Bonding: {}/180", member.bond_timer));
                    }
                } else {
                    let food_stock = registry
                        .factions
                        .get(&member.faction_id)
                        .map_or(0.0, |f| f.storage.food_total());
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
                    ui.label(format!(
                        "Faction: #{} (food: {:.1}){}",
                        member.faction_id, food_stock, raid_info
                    ));
                    // Lineage + culture style summary
                    if let Some(f) = registry.factions.get(&member.faction_id) {
                        ui.label(
                            egui::RichText::new(format!(
                                "Lineage: {} (gen {})",
                                f.lineage.root, f.lineage.generation
                            ))
                            .color(egui::Color32::from_gray(180))
                            .size(11.0),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "Founder: {} • Style: {}",
                                f.lineage.founder,
                                f.culture.style.label()
                            ))
                            .color(egui::Color32::from_gray(180))
                            .size(11.0),
                        );
                    }
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
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(140.0)
                                .fill(color),
                        );
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
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(140.0)
                                .fill(color),
                        );
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
                                ui.add(
                                    egui::ProgressBar::new(frac)
                                        .desired_width(100.0)
                                        .fill(color),
                                );
                                ui.label(format!("{}/{}", limb.current, limb.max));
                            });
                        }
                    });
                }

                ui.separator();
                ui.label("Needs:");
                needs_bar(ui, "Hunger", needs.hunger);
                needs_bar(ui, "Sleep", needs.sleep);
                needs_bar(ui, "Shelter", needs.shelter);
                needs_bar(ui, "Safety", needs.safety);
                needs_bar(ui, "Social", needs.social);
                needs_bar(ui, "Repro", needs.reproduction);

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

                let task_name = match ai.task_id {
                    j if j == TaskKind::Idle as u16 => "Idle",
                    j if j == TaskKind::Gather as u16 => "Gatherer",
                    j if j == TaskKind::Trader as u16 => "Trader",
                    j if j == TaskKind::Raid as u16 => "Raider",
                    j if j == TaskKind::Defend as u16 => "Defender",
                    j if j == TaskKind::Planter as u16 => "Planter",
                    j if j == TaskKind::Hunter as u16 => "Hunter",
                    j if j == TaskKind::Scavenge as u16 => "Scavenger",
                    j if j == TaskKind::Construct as u16 => "Builder",
                    j if j == TaskKind::ConstructBed as u16 => "Building Bed",
                    j if j == TaskKind::Deconstruct as u16 => "Deconstructing",
                    j if j == TaskKind::DepositResource as u16 => "Depositing Resources",
                    j if j == TaskKind::Socialize as u16 => "Socializing",
                    j if j == TaskKind::Reproduce as u16 => "Reproducing",
                    j if j == TaskKind::Explore as u16 => "Explorer",
                    j if j == TaskKind::Dig as u16 => "Digger",
                    j if j == TaskKind::Sleep as u16 => "Sleeper",
                    j if j == TaskKind::Eat as u16 => "Eating",
                    j if j == TaskKind::WithdrawFood as u16 => "Withdrawing Food",
                    j if j == TaskKind::TameAnimal as u16 => "Taming",
                    j if j == TaskKind::Craft as u16 => "Crafter",
                    j if j == TaskKind::Lead as u16 => "Leading",
                    j if j == TaskKind::Terraform as u16 => "Levelling Ground",
                    _ => "Unemployed",
                };
                ui.label(format!("Task: {}", task_name));

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

                        if ai.task_id == TaskKind::Gather as u16 {
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
                            work_str =
                                format!("{} ({}%)", work_str, (ai.work_progress as u32 * 100) / 30);
                        // 30 is base stone work_ticks
                        } else if ai.task_id == TaskKind::Planter as u16 {
                            work_str =
                                format!("Planting Seeds ({}%)", (ai.work_progress as u32 * 100) / 40);
                        // 40 is TICKS_FARMER_PLANT
                        } else if ai.task_id == TaskKind::Raid as u16 {
                            work_str = "Stealing Goods".to_string();
                        } else if ai.task_id == TaskKind::Scavenge as u16 {
                            work_str = "Picking up item".to_string();
                        }

                        work_str
                    }
                };
                ui.label(format!("State: {}", state_desc));
                ui.label(format!(
                    "Target: {}, {}",
                    ai.target_tile.0, ai.target_tile.1
                ));

                if let Some(o) = order {
                    ui.label(
                        egui::RichText::new(format!(
                            "Order: {} \u{2192} ({}, {})",
                            o.order.label(),
                            o.target_tile.0,
                            o.target_tile.1
                        ))
                        .color(egui::Color32::from_rgb(255, 220, 100)),
                    );
                }

                ui.separator();
                if let Some(ap) = active_plan {
                    if let Some(plan_def) = plan_registry.0.iter().find(|p| p.id == ap.plan_id) {
                        ui.label(
                            egui::RichText::new(format!("Active Plan: {}", plan_def.name))
                                .strong()
                                .color(egui::Color32::LIGHT_BLUE),
                        );
                        ui.label(format!(
                            "  Step: {}/{}",
                            ap.current_step + 1,
                            plan_def.steps.len()
                        ));
                        ui.label(format!("  Reward: {:.2}", ap.reward_acc));
                    }
                } else {
                    ui.label(
                        egui::RichText::new("Active Plan: None")
                            .italics()
                            .color(egui::Color32::GRAY),
                    );
                }

                ui.separator();
                egui::CollapsingHeader::new(
                    egui::RichText::new("Pathing")
                        .strong()
                        .color(egui::Color32::from_rgb(180, 180, 240)),
                )
                .default_open(true)
                .show(ui, |ui| {
                    let cur_tx =
                        (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
                    let cur_ty =
                        (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
                    if let Ok(pf) = path_params.path_follows.get(entity) {
                        let (status_text, status_color) = match pf.status {
                            FollowStatus::Idle => ("Idle", egui::Color32::GRAY),
                            FollowStatus::Pending => ("Pending", egui::Color32::YELLOW),
                            FollowStatus::Following => (
                                "Following",
                                egui::Color32::from_rgb(120, 220, 120),
                            ),
                            FollowStatus::Failed(FailReason::Unreachable) => {
                                ("Failed: Unreachable", egui::Color32::from_rgb(240, 100, 100))
                            }
                            FollowStatus::Failed(FailReason::BudgetExhausted) => (
                                "Failed: BudgetExhausted",
                                egui::Color32::from_rgb(240, 100, 100),
                            ),
                            FollowStatus::Failed(FailReason::NoRoute) => {
                                ("Failed: NoRoute", egui::Color32::from_rgb(240, 100, 100))
                            }
                        };
                        ui.label(
                            egui::RichText::new(format!("Status: {}", status_text)).color(status_color),
                        );
                        ui.label(format!(
                            "Goal: ({}, {}, {})",
                            pf.goal.0, pf.goal.1, pf.goal.2
                        ));
                        ui.label(format!("At:   ({}, {})", cur_tx, cur_ty));
                        ui.label(format!(
                            "Chunk route: {}/{}",
                            pf.route_cursor.min(pf.chunk_route.len() as u8),
                            pf.chunk_route.len()
                        ));
                        if !pf.chunk_route.is_empty() {
                            let preview: Vec<String> = pf
                                .chunk_route
                                .iter()
                                .take(6)
                                .map(|c| format!("({},{})", c.0, c.1))
                                .collect();
                            let suffix = if pf.chunk_route.len() > 6 { " …" } else { "" };
                            ui.label(
                                egui::RichText::new(format!(
                                    "  {}{}",
                                    preview.join(" → "),
                                    suffix
                                ))
                                .size(11.0)
                                .color(egui::Color32::GRAY),
                            );
                        }
                        ui.label(format!(
                            "Segment: {}/{}",
                            pf.segment_cursor.min(pf.segment_path.len() as u16),
                            pf.segment_path.len()
                        ));
                        ui.label(format!(
                            "stuck_ticks: {}  •  last_replan: {}",
                            pf.stuck_ticks, pf.last_replan_tick,
                        ));
                        ui.label(
                            egui::RichText::new(format!(
                                "request_id: {}  •  planning_gen: {}  •  pending: {}",
                                pf.request_id,
                                pf.planning_generation,
                                path_params.path_queue.is_pending(entity),
                            ))
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                        );
                        let recent: Vec<String> = pf
                            .recent_tiles
                            .iter()
                            .filter(|(x, y, _)| !(*x == i16::MIN && *y == i16::MIN))
                            .map(|(x, y, z)| format!("({},{},{})", x, y, z))
                            .collect();
                        ui.label(
                            egui::RichText::new(format!("Recent tiles: {}", recent.join(" ")))
                                .size(11.0)
                                .color(egui::Color32::GRAY),
                        );
                        if let Some(reason) = pf.last_fail_reason {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Last failure: {:?} @ tick {}",
                                    reason, pf.last_fail_tick
                                ))
                                .color(egui::Color32::from_rgb(240, 160, 100)),
                            );
                        }

                        let agent_failures: Vec<_> =
                            path_params.failure_log.for_agent(entity).take(8).collect();
                        if !agent_failures.is_empty() {
                            egui::CollapsingHeader::new(format!(
                                "Failure history ({})",
                                agent_failures.len()
                            ))
                            .default_open(false)
                            .show(ui, |ui| {
                                for rec in agent_failures {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "tick {}  goal ({},{},{})  {:?}",
                                            rec.tick, rec.goal.0, rec.goal.1, rec.goal.2, rec.reason
                                        ))
                                        .size(11.0)
                                        .color(egui::Color32::GRAY),
                                    );
                                }
                            });
                        }

                        let mut do_reset = false;
                        ui.horizontal(|ui| {
                            if ui.button("Force replan").clicked() {
                                do_reset = true;
                            }
                        });
                        if do_reset {
                            if let Ok(mut pf) = path_params.path_follows.get_mut(entity) {
                                pf.status = FollowStatus::Idle;
                                pf.chunk_route.clear();
                                pf.route_cursor = 0;
                                pf.segment_path.clear();
                                pf.segment_cursor = 0;
                                pf.stuck_ticks = 0;
                                pf.recent_tiles = [(i16::MIN, i16::MIN, 0); 4];
                            }
                            path_params.path_queue.cancel_for_agent(entity);
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("No PathFollow component")
                                .italics()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });

                if let (Some(kp), Some(net)) = (known_plans, utility_net) {
                    egui::CollapsingHeader::new("Available Plans").show(ui, |ui| {
                        let state = build_state_vec(needs, agent, skills, member, memory, &calendar);
                        let cur_tx =
                            (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
                        let cur_ty =
                            (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
                        let camp_pos = registry.home_tile(member.faction_id);

                        for plan_def in &plan_registry.0 {
                            // Viability checks
                            let serving_goal = plan_def.serves_goals.contains(goal);
                            let known = kp.knows(plan_def.id);
                            let tech_unlocked = plan_def.tech_gate.map_or(true, |tid| {
                                registry
                                    .factions
                                    .get(&member.faction_id)
                                    .map(|f| f.techs.has(tid))
                                    .unwrap_or(false)
                            });
                            let first_step_ready = plan_def
                                .steps
                                .first()
                                .and_then(|&sid| step_registry.0.iter().find(|s| s.id == sid))
                                .map_or(true, |s| s.preconditions.is_satisfied(agent, needs.hunger));

                            let mut rejection = None;
                            if !serving_goal {
                                rejection = Some("Wrong Goal");
                            } else if !known {
                                rejection = Some("Not Learned");
                            } else if !tech_unlocked {
                                rejection = Some("Tech Locked");
                            } else if !first_step_ready {
                                rejection = Some("Preconditions Unmet");
                            }

                            if let Some(reason) = rejection {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}: Rejected ({})",
                                        plan_def.name, reason
                                    ))
                                    .small()
                                    .color(egui::Color32::from_gray(120)),
                                );
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
                                    let dist_agent = (target.0 as i32 - cur_tx).abs()
                                        + (target.1 as i32 - cur_ty).abs();
                                    let dist_camp = camp_pos.map_or(0, |c| {
                                        (target.0 as i32 - c.0 as i32).abs()
                                            + (target.1 as i32 - c.1 as i32).abs()
                                    });
                                    let penalty = (dist_agent + dist_camp) as f32 * 0.002;
                                    score -= penalty;
                                    bonus_str += &format!(" (-{:.2} dist)", penalty);
                                } else {
                                    score -= 0.5;
                                    bonus_str += " (-0.5 no target)";
                                }

                                ui.horizontal(|ui| {
                                    ui.label(format!("{}: ", plan_def.name));
                                    ui.label(
                                        egui::RichText::new(format!("{:.2}", score))
                                            .color(egui::Color32::YELLOW),
                                    );
                                    ui.label(
                                        egui::RichText::new(bonus_str)
                                            .small()
                                            .color(egui::Color32::from_gray(160)),
                                    );
                                });
                            }
                        }
                    });
                }

                if let Some(rel) = rel_mem {
                    egui::CollapsingHeader::new("Relationships").show(ui, |ui| {
                        let mut entries: Vec<&RelEntry> =
                            rel.entries.iter().filter_map(|s| s.as_ref()).collect();
                        entries.sort_unstable_by(|a, b| b.affinity.cmp(&a.affinity));

                        if entries.is_empty() {
                            ui.label(
                                egui::RichText::new("No relationships yet")
                                    .italics()
                                    .color(egui::Color32::GRAY),
                            );
                        }
                        for entry in entries {
                            let name = name_query
                                .get(entry.entity)
                                .map(|n| n.as_str())
                                .unwrap_or("Unknown");
                            let normalized = (entry.affinity as f32 + 128.0) / 255.0;
                            let color = if entry.affinity >= 0 {
                                egui::Color32::from_rgb(
                                    30,
                                    (normalized * 510.0).min(255.0) as u8,
                                    30,
                                )
                            } else {
                                egui::Color32::from_rgb(
                                    ((1.0 - normalized) * 510.0).min(255.0) as u8,
                                    30,
                                    30,
                                )
                            };
                            ui.horizontal(|ui| {
                                ui.label(format!("{:12}", name));
                                ui.add(
                                    egui::ProgressBar::new(normalized)
                                        .desired_width(100.0)
                                        .fill(color),
                                );
                                ui.label(format!("{:+}", entry.affinity));
                            });
                        }
                    });
                }
            });
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
