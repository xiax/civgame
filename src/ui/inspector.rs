use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::pathfinding::path_request::{
    FailReason, FailureLog, FollowStatus, PathFollow, PathRequestQueue,
};
use crate::simulation::combat::{Body, BodyPart, Health};
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction, SOLO};
use crate::simulation::goals::{AgentGoal, GoalReason, Personality};
use crate::simulation::memory::{AgentMemory, RelEntry, RelationshipMemory};
use crate::simulation::mood::Mood;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI, PlayerOrder, Profession};
use crate::simulation::plan::{
    build_state_vec, score_weighted, ActivePlan, KnownPlans, PlanHistory, PlanOutcome,
    PlanRegistry, StepRegistry,
};
use crate::simulation::plants::{Plant, PlantMap};
use crate::simulation::reproduction::{
    BiologicalSex, CoSleepTracker, MaleConceptionCooldown, Pregnancy, PREGNANCY_TICKS,
};
use crate::simulation::skills::{SkillKind, Skills, SKILL_COUNT};
use crate::simulation::stats::{self, Stats};
use crate::simulation::tasks::{task_kind_label, TaskKind};
use crate::world::chunk::ChunkMap;
use crate::world::seasons::Calendar;
use crate::world::seasons::TICKS_PER_SEASON;
use crate::world::tile::TileKind;

use super::selection::SelectedEntity;

#[derive(SystemParam)]
pub struct PathInspectorParams<'w, 's> {
    pub failure_log: Res<'w, FailureLog>,
    pub path_queue: ResMut<'w, PathRequestQueue>,
    pub path_follows: Query<'w, 's, &'static mut PathFollow>,
}

#[derive(SystemParam)]
pub struct JobInspectorParams<'w, 's> {
    pub claim_query: Query<'w, 's, &'static crate::simulation::jobs::JobClaim>,
    pub commands: EventWriter<'w, crate::simulation::jobs::JobBoardCommand>,
    pub board: Res<'w, crate::simulation::jobs::JobBoard>,
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
    mut job_params: JobInspectorParams,
    repro_query: Query<(
        Option<&Pregnancy>,
        Option<&CoSleepTracker>,
        Option<&MaleConceptionCooldown>,
    )>,
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
            Option<&Stats>,
        ),
        (
            Option<&Health>,
            Option<&Body>,
            Option<&PlayerOrder>,
            &Transform,
            Option<&ActivePlan>,
            Option<&KnownPlans>,
            Option<&AgentMemory>,
            Option<&GoalReason>,
            Option<&crate::simulation::carry::Carrier>,
            Option<&PlanHistory>,
            Option<&crate::simulation::items::Equipment>,
        ),
    )>,
) {
    let Some(entity) = selected.0 else { return };
    let Ok((
        (needs, mood, skills, ai, agent, goal, personality, sex, member, profession, stats),
        (
            health,
            body,
            order,
            transform,
            active_plan,
            known_plans,
            memory,
            goal_reason,
            carrier,
            plan_history,
            equipment,
        ),
    )) = query.get(entity)
    else {
        return;
    };
    let rel_mem = rel_query.get(entity).ok();
    let (preg_opt, cosleep_opt, male_cd_opt) = repro_query
        .get(entity)
        .map(|(p, c, m)| (p, c, m))
        .unwrap_or((None, None, None));

    egui::Window::new("Inspector")
        .default_pos([10.0, 10.0])
        .default_width(560.0)
        .default_height(720.0)
        .min_width(400.0)
        .resizable(true)
        .show(contexts.ctx_mut(), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::CollapsingHeader::new(egui::RichText::new("Identity").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("Mood: {} ({})", mood.label(), mood.0));
                            ui.separator();
                            ui.label(sex.name());
                        });
                        ui.label(format!("Personality: {}", personality.name()));
                        ui.label(format!("Profession: {:?}", profession));
                        ui.horizontal(|ui| {
                            ui.label(format!("Goal: {}", goal.name()));
                            let reason_text = goal_reason.map(|r| r.0).unwrap_or("—");
                            ui.label(
                                egui::RichText::new(format!(" ({})", reason_text))
                                    .small()
                                    .color(egui::Color32::from_gray(160)),
                            );
                        });
                        if let Ok(claim) = job_params.claim_query.get(entity) {
                            ui.horizontal(|ui| {
                                ui.label(format!(
                                    "Job: {} (#{})",
                                    claim.kind.name(),
                                    claim.job_id
                                ));
                                if ui.button("Release").clicked() {
                                    job_params.commands.send(
                                        crate::simulation::jobs::JobBoardCommand::Cancel(
                                            claim.job_id,
                                        ),
                                    );
                                }
                            });
                            // Extra job details: source, progress, fail count
                            if let Some(postings) = job_params.board.postings.get(&claim.faction_id) {
                                if let Some(post) = postings.iter().find(|p| p.id == claim.job_id) {
                                    ui.horizontal(|ui| {
                                        ui.label(format!("Source: {:?}", post.source))
                                            .on_hover_text("Reason/Source of this job posting");
                                        ui.separator();
                                        ui.label(format!(
                                            "Progress: {:.0}%",
                                            post.progress.fraction() * 100.0
                                        ));
                                        if claim.fail_count > 0 {
                                            ui.separator();
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "Struggles: {}/3",
                                                    claim.fail_count
                                                ))
                                                .color(egui::Color32::from_rgb(255, 100, 100)),
                                            )
                                            .on_hover_text("Number of times worker got stuck or timed out");
                                        }
                                    });
                                }
                            }
                        }

                        if member.faction_id == SOLO {
                            ui.label("Faction: Solo");
                            if member.bond_timer > 0 {
                                ui.label(format!("Bonding: {}/180", member.bond_timer));
                            } else {
                                ui.label(
                                    egui::RichText::new("Bonding: —")
                                        .color(egui::Color32::from_gray(140)),
                                );
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
                            let (lineage_text, founder_text) =
                                match registry.factions.get(&member.faction_id) {
                                    Some(f) => (
                                        format!(
                                            "Lineage: {} (gen {})",
                                            f.lineage.root, f.lineage.generation
                                        ),
                                        format!(
                                            "Founder: {} • Style: {}",
                                            f.lineage.founder,
                                            f.culture.style.label()
                                        ),
                                    ),
                                    None => ("Lineage: —".to_string(), "Founder: —".to_string()),
                                };
                            ui.label(
                                egui::RichText::new(lineage_text)
                                    .color(egui::Color32::from_gray(180))
                                    .size(11.0),
                            );
                            ui.label(
                                egui::RichText::new(founder_text)
                                    .color(egui::Color32::from_gray(180))
                                    .size(11.0),
                            );
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Health").strong())
                    .default_open(true)
                    .show(ui, |ui| {
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
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Needs").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        needs_bar(ui, "Hunger", needs.hunger);
                        needs_bar(ui, "Sleep", needs.sleep);
                        needs_bar(ui, "Shelter", needs.shelter);
                        needs_bar(ui, "Safety", needs.safety);
                        needs_bar(ui, "Social", needs.social);
                        needs_bar(ui, "Repro", needs.reproduction);
                        willpower_bar(ui, needs.willpower);
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Skills").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        for i in 0..SKILL_COUNT {
                            let kind = unsafe { std::mem::transmute::<u8, SkillKind>(i as u8) };
                            ui.label(format!("  {}: {}", kind.name(), skills.0[i]));
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Stats").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(s) = stats {
                            let row = |ui: &mut egui::Ui, label: &str, score: u8| {
                                let m = stats::modifier(score);
                                let sign = if m >= 0 { "+" } else { "" };
                                ui.label(format!("  {}: {} ({}{})", label, score, sign, m));
                            };
                            row(ui, "STR", s.strength);
                            row(ui, "DEX", s.dexterity);
                            row(ui, "CON", s.constitution);
                            row(ui, "INT", s.intelligence);
                            row(ui, "WIS", s.wisdom);
                            row(ui, "CHA", s.charisma);
                        } else {
                            ui.label("  (no stats)");
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Currency & Inventory").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(format!("Currency: {:.1}", agent.currency));

                        let cur_g = agent.current_weight_g();
                        let cap_g = agent.capacity_g();
                        ui.label(format!(
                            "Inventory ({:.1} / {:.1} kg):",
                            cur_g as f32 / 1000.0,
                            cap_g as f32 / 1000.0,
                        ));
                        let frac = if cap_g > 0 {
                            (cur_g as f32 / cap_g as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(180.0)
                                .text(format!("{:.1} kg", cur_g as f32 / 1000.0)),
                        );
                        egui::ScrollArea::vertical()
                            .id_salt("inv")
                            .max_height(100.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (item, qty) in &agent.inventory {
                                    if *qty > 0 {
                                        ui.label(format!(
                                            "  {}: {} ({:.2} kg)",
                                            item.label(),
                                            qty,
                                            item.stack_weight_g(*qty) as f32 / 1000.0
                                        ));
                                    }
                                }
                            });

                        let (free_hands_str, left_slot, right_slot) = match carrier {
                            Some(c) => (format!("{} free", c.free_hands()), c.left, c.right),
                            None => ("—".to_string(), None, None),
                        };
                        ui.label(format!("In hands: {}", free_hands_str));
                        let two_handed = left_slot.map_or(false, |s| s.two_handed);
                        match left_slot {
                            Some(stack) => {
                                let tag = if stack.two_handed { " [2H]" } else { "" };
                                ui.label(format!(
                                    "  L: {} ×{}{} ({:.2} kg)",
                                    stack.item.label(),
                                    stack.qty,
                                    tag,
                                    stack.weight_g() as f32 / 1000.0,
                                ));
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("  L: —")
                                        .color(egui::Color32::from_gray(140)),
                                );
                            }
                        }
                        if two_handed {
                            ui.label(
                                egui::RichText::new("  R: (held with two hands)")
                                    .color(egui::Color32::from_gray(140)),
                            );
                        } else {
                            match right_slot {
                                Some(stack) => {
                                    let tag = if stack.two_handed { " [2H]" } else { "" };
                                    ui.label(format!(
                                        "  R: {} ×{}{} ({:.2} kg)",
                                        stack.item.label(),
                                        stack.qty,
                                        tag,
                                        stack.weight_g() as f32 / 1000.0,
                                    ));
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new("  R: —")
                                            .color(egui::Color32::from_gray(140)),
                                    );
                                }
                            }
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Task & State").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        let task_name = task_kind_label(ai.task_id);
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
                                    work_str = format!(
                                        "{} ({}%)",
                                        work_str,
                                        (ai.work_progress as u32 * 100) / 30
                                    );
                                // 30 is base stone work_ticks
                                } else if ai.task_id == TaskKind::Planter as u16 {
                                    work_str = format!(
                                        "Planting Seeds ({}%)",
                                        (ai.work_progress as u32 * 100) / 40
                                    );
                                // 40 is TICKS_FARMER_PLANT
                                } else if ai.task_id == TaskKind::Raid as u16 {
                                    work_str = "Stealing Goods".to_string();
                                } else if ai.task_id == TaskKind::Scavenge as u16 {
                                    work_str = "Picking up item".to_string();
                                } else if ai.task_id == TaskKind::WithdrawMaterial as u16 {
                                    if let Some(good) = ai.withdraw_good {
                                        work_str = format!(
                                            "Withdrawing {:?} \u{00d7} {}",
                                            good, ai.withdraw_qty
                                        );
                                    } else {
                                        work_str = "Withdrawing".to_string();
                                    }
                                }

                                work_str
                            }
                        };
                        ui.label(format!("State: {}", state_desc));
                        ui.label(format!(
                            "Target: {}, {}",
                            ai.target_tile.0, ai.target_tile.1
                        ));
                        if let Some(good) = ai.withdraw_good {
                            ui.label(format!(
                                "Withdraw intent: {:?} \u{00d7} {} from ({}, {})",
                                good, ai.withdraw_qty, ai.dest_tile.0, ai.dest_tile.1
                            ));
                        }

                        match order {
                            Some(o) => {
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
                            None => {
                                ui.label(
                                    egui::RichText::new("Order: —")
                                        .color(egui::Color32::from_gray(120)),
                                );
                            }
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Active Plan").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        let active_pair = active_plan.and_then(|ap| {
                            plan_registry
                                .0
                                .iter()
                                .find(|p| p.id == ap.plan_id)
                                .map(|pd| (ap, pd))
                        });
                        match active_pair {
                            Some((ap, plan_def)) => {
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
                            None => {
                                ui.label(
                                    egui::RichText::new("Active Plan: None")
                                        .italics()
                                        .color(egui::Color32::GRAY),
                                );
                                ui.label(
                                    egui::RichText::new("  Step: —")
                                        .italics()
                                        .color(egui::Color32::GRAY),
                                );
                                ui.label(
                                    egui::RichText::new("  Reward: —")
                                        .italics()
                                        .color(egui::Color32::GRAY),
                                );
                            }
                        }

                        if let Some(history) = plan_history {
                            ui.separator();
                            ui.label(
                                egui::RichText::new("Recent Outcomes (oldest → newest):").strong(),
                            );
                            // Walk the ring oldest-to-newest starting at `head`.
                            let len = history.entries.len();
                            let mut any = false;
                            for i in 0..len {
                                let idx = (history.head as usize + i) % len;
                                let Some((plan_id, outcome, _tick)) = history.entries[idx] else {
                                    continue;
                                };
                                any = true;
                                let plan_name = plan_registry
                                    .0
                                    .iter()
                                    .find(|p| p.id == plan_id)
                                    .map(|p| p.name)
                                    .unwrap_or("?");
                                let (label, color) = match outcome {
                                    PlanOutcome::Success => {
                                        ("Success", egui::Color32::from_rgb(120, 220, 120))
                                    }
                                    PlanOutcome::FailedNoTarget => {
                                        ("FailedNoTarget", egui::Color32::from_rgb(240, 100, 100))
                                    }
                                    PlanOutcome::FailedPrecondition => (
                                        "FailedPrecondition",
                                        egui::Color32::from_rgb(240, 100, 100),
                                    ),
                                    PlanOutcome::Aborted => {
                                        ("Aborted", egui::Color32::from_rgb(220, 200, 80))
                                    }
                                    PlanOutcome::Interrupted => {
                                        ("Interrupted", egui::Color32::from_rgb(220, 200, 80))
                                    }
                                };
                                ui.label(
                                    egui::RichText::new(format!("  {}: {}", plan_name, label))
                                        .small()
                                        .color(color),
                                );
                            }
                            if !any {
                                ui.label(
                                    egui::RichText::new("  (none)")
                                        .small()
                                        .italics()
                                        .color(egui::Color32::GRAY),
                                );
                            }
                        }
                    });
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
                        let red = egui::Color32::from_rgb(240, 100, 100);
                        let (status_text, status_color): (String, egui::Color32) = match pf.status {
                            FollowStatus::Idle => ("Idle".into(), egui::Color32::GRAY),
                            FollowStatus::Pending => ("Pending".into(), egui::Color32::YELLOW),
                            FollowStatus::Following => {
                                ("Following".into(), egui::Color32::from_rgb(120, 220, 120))
                            }
                            FollowStatus::Failed(reason) => {
                                let label = match pf.last_fail_subreason {
                                    Some(sub) => format!("Failed: {}", sub.label()),
                                    None => match reason {
                                        FailReason::Unreachable => "Failed: Unreachable".into(),
                                        FailReason::BudgetExhausted => {
                                            "Failed: BudgetExhausted".into()
                                        }
                                        FailReason::NoRoute => "Failed: NoRoute".into(),
                                    },
                                };
                                (label, red)
                            }
                        };
                        ui.label(
                            egui::RichText::new(format!("Status: {}", status_text))
                                .color(status_color),
                        );
                        ui.label(format!(
                            "Goal: ({}, {}, {})",
                            pf.goal.0, pf.goal.1, pf.goal.2
                        ));
                        ui.label(format!("At:   ({}, {}, {})", cur_tx, cur_ty, ai.current_z));
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
                                egui::RichText::new(format!("  {}{}", preview.join(" → "), suffix))
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
                        let total_fails = pf.fail_count_unreachable_conn
                            + pf.fail_count_unreachable_astar
                            + pf.fail_count_budget
                            + pf.fail_count_no_route_router
                            + pf.fail_count_no_route_continuity;
                        let fail_color = if total_fails > 0 {
                            egui::Color32::from_rgb(240, 160, 100)
                        } else {
                            egui::Color32::GRAY
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "Failures: conn {}  astar {}  budget {}  router {}  cont {}",
                                pf.fail_count_unreachable_conn,
                                pf.fail_count_unreachable_astar,
                                pf.fail_count_budget,
                                pf.fail_count_no_route_router,
                                pf.fail_count_no_route_continuity,
                            ))
                            .size(11.0)
                            .color(fail_color),
                        );

                        if let Some(dump) = pf.last_astar_dump.as_deref() {
                            egui::CollapsingHeader::new("Last A* Unreachable dump")
                                .default_open(true)
                                .show(ui, |ui| {
                                    egui::ScrollArea::both()
                                        .max_height(440.0)
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(dump)
                                                        .monospace()
                                                        .size(12.0),
                                                )
                                                .wrap_mode(egui::TextWrapMode::Extend),
                                            );
                                        });
                                });
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
                                            "tick {}  goal ({},{},{})  {}",
                                            rec.tick,
                                            rec.goal.0,
                                            rec.goal.1,
                                            rec.goal.2,
                                            rec.subreason.label()
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

                if let Some(kp) = known_plans {
                    egui::CollapsingHeader::new("Available Plans").show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("plans")
                            .max_height(160.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // Inspector preview doesn't have the spatial queries to
                                // compute visibility; pass zeros. Storage stocks come
                                // from FactionRegistry directly so the SI_STORAGE_* slots
                                // reflect reality. Live agent scoring still uses real
                                // visibility in plan_execution_system.
                                let storage_opt = registry
                                    .factions
                                    .get(&member.faction_id)
                                    .map(|f| &f.storage);
                                let state = build_state_vec(
                                    needs,
                                    agent,
                                    skills,
                                    member,
                                    memory,
                                    &calendar,
                                    plan_history,
                                    storage_opt,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    false,
                                );
                                let cur_tx = (transform.translation.x
                                    / crate::world::terrain::TILE_SIZE)
                                    .floor() as i32;
                                let cur_ty = (transform.translation.y
                                    / crate::world::terrain::TILE_SIZE)
                                    .floor() as i32;
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
                                        .and_then(|&sid| {
                                            step_registry.0.iter().find(|s| s.id == sid)
                                        })
                                        .map_or(true, |s| {
                                            let carrier_default =
                                                crate::simulation::carry::Carrier::default();
                                            let c = carrier.unwrap_or(&carrier_default);
                                            s.preconditions.is_satisfied(
                                                agent,
                                                c,
                                                equipment,
                                                needs.hunger,
                                            )
                                        });

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
                                        let base = score_weighted(&state, plan_def);
                                        let mut score = base;
                                        let mut bonus_str = format!(
                                            "= base {:.2} ({:+.2} weights, {:+.2} bias)",
                                            base,
                                            base - plan_def.bias,
                                            plan_def.bias,
                                        );
                                        if plan_def.id == ai.last_plan_id {
                                            score += 0.0;
                                            bonus_str += " +0.0 persist";
                                        }

                                        let target_tile =
                                            plan_def.memory_target_kind.and_then(|k| {
                                                memory.and_then(|m| {
                                                    m.best_for_dist_weighted(k, (cur_tx, cur_ty))
                                                })
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
                                            bonus_str += &format!(" -{:.2} dist", penalty);
                                        } else if plan_def.memory_target_kind.is_some() {
                                            score -= 0.1;
                                            bonus_str += " -0.1 no target";
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
                    });
                }

                egui::CollapsingHeader::new(egui::RichText::new("Reproduction").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(p) = preg_opt {
                            let elapsed = PREGNANCY_TICKS.saturating_sub(p.ticks_remaining);
                            let seasons_remaining =
                                p.ticks_remaining as f32 / TICKS_PER_SEASON as f32;
                            ui.label(
                                egui::RichText::new(format!(
                                    "Pregnant: {}/{} ticks ({:.2} seasons remaining)",
                                    elapsed, PREGNANCY_TICKS, seasons_remaining
                                ))
                                .color(egui::Color32::from_rgb(220, 130, 200)),
                            );
                            let father_name = p
                                .father
                                .and_then(|f| name_query.get(f).ok())
                                .map(|n| n.as_str().to_string())
                                .unwrap_or_else(|| "—".to_string());
                            ui.label(format!("Father: {}", father_name));
                        } else if *sex == BiologicalSex::Female {
                            ui.label(
                                egui::RichText::new("Not pregnant")
                                    .color(egui::Color32::from_gray(140)),
                            );
                        }
                        if let Some(cs) = cosleep_opt {
                            let partner_name = cs
                                .partner
                                .and_then(|e| name_query.get(e).ok())
                                .map(|n| n.as_str().to_string())
                                .unwrap_or_else(|| "—".to_string());
                            ui.label(format!(
                                "Co-sleep partner: {} (ticks: {})",
                                partner_name, cs.ticks_co_slept
                            ));
                        }
                        if *sex == BiologicalSex::Male {
                            if let Some(cd) = male_cd_opt {
                                if cd.0 > 0 {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Refractory: {} ticks remaining",
                                            cd.0
                                        ))
                                        .color(egui::Color32::from_rgb(180, 140, 120)),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new("Refractory: ready")
                                            .color(egui::Color32::from_gray(140)),
                                    );
                                }
                            }
                        }
                    });

                if let Some(rel) = rel_mem {
                    egui::CollapsingHeader::new("Relationships").show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("rels")
                            .max_height(140.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
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

/// Like `needs_bar` but inverted: full bar = green = good (high willpower);
/// empty bar = red = drained. Bar fills proportionally to the actual value
/// rather than to the distress component.
fn willpower_bar(ui: &mut egui::Ui, value: f32) {
    ui.horizontal(|ui| {
        ui.label(format!("{:8}", "Willpower"));
        let progress = (value / 255.0).clamp(0.0, 1.0);
        let color = egui::Color32::from_rgb(
            (255.0 * (1.0 - progress)) as u8,
            (255.0 * progress) as u8,
            30,
        );
        let bar = egui::ProgressBar::new(progress)
            .desired_width(140.0)
            .fill(color);
        ui.add(bar);
        ui.label(format!("{value}"));
    });
}
