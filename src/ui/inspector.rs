use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::agent::EconomicAgent;
use crate::economy::item::Item;
use crate::pathfinding::path_request::{
    FailReason, FailureLog, FollowStatus, PathFollow, PathRequestQueue,
};
use crate::simulation::carry::Carrier;
use crate::simulation::combat::{Body, BodyPart, Health};
use crate::simulation::corpse::Corpse;
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction, SOLO};
use crate::simulation::goals::{AgentGoal, GoalReason, Personality};
use crate::simulation::htn::{MethodHistory, MethodOutcome, METHOD_HISTORY_TTL_TICKS};
use crate::simulation::items::{
    spawn_or_merge_ground_item_full, valid_equip_slots, Equipment, EquipmentSlot, GroundItem,
};
use crate::simulation::memory::{RelEntry, RelationshipMemory};
use crate::simulation::mood::Mood;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::plants::{Plant, PlantMap};
use crate::simulation::reproduction::{
    BiologicalSex, CoSleepTracker, MaleConceptionCooldown, Pregnancy, PREGNANCY_TICKS,
};
use crate::simulation::schedule::SimClock;
use crate::simulation::skills::{SkillKind, Skills, SKILL_COUNT};
use crate::simulation::stats::{self, Stats};
use crate::simulation::tasks::{task_kind_label, TaskKind};
use crate::world::chunk::ChunkMap;
use crate::world::seasons::Calendar;
use crate::world::seasons::TICKS_PER_SEASON;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;

use super::selection::SelectedEntity;

/// Pending inventory/equipment action queued by the inspector UI, executed by
/// `inspector_action_system` on the next frame.
#[derive(Resource, Default)]
pub struct PendingInspectorAction(pub Option<InspectorActionKind>);

pub enum InspectorActionKind {
    DropInventoryItem {
        target: Entity,
        item: Item,
        qty: u32,
    },
    DropInvItemOne {
        target: Entity,
        item: Item,
    },
    DropLeftHand {
        target: Entity,
    },
    DropRightHand {
        target: Entity,
    },
    EquipItem {
        target: Entity,
        item: Item,
        from_hands: bool,
        slot: EquipmentSlot,
    },
    UnequipSlot {
        target: Entity,
        slot: EquipmentSlot,
    },
    /// Selected agent broadcasts a tech to nearby same-faction adults via
    /// `apply_lecture_request_system` (Economy).
    HoldLecture {
        lecturer: Entity,
        tech: crate::simulation::technology::TechId,
    },
    /// Selected agent reads an inventory tablet/book whose `tech_payload`
    /// matches `tech`. Routed by `apply_player_knowledge_orders_system`.
    ReadItem {
        reader: Entity,
        tech: crate::simulation::technology::TechId,
    },
    /// Player asks the faction to craft a Clay Tablet encoding `tech`.
    /// Posted by `chief_tablet_posting_system` via `PlayerCraftRequest`.
    EncodeTablet {
        tech: crate::simulation::technology::TechId,
    },
}

#[derive(SystemParam)]
pub struct PathInspectorParams<'w, 's> {
    pub failure_log: Res<'w, FailureLog>,
    pub path_queue: ResMut<'w, PathRequestQueue>,
    pub path_follows: Query<'w, 's, &'static mut PathFollow>,
    pub pending_action: ResMut<'w, PendingInspectorAction>,
}

#[derive(SystemParam)]
pub struct JobInspectorParams<'w, 's> {
    pub claim_query: Query<'w, 's, &'static crate::simulation::jobs::JobClaim>,
    pub commands: EventWriter<'w, crate::simulation::jobs::JobBoardCommand>,
    pub board: Res<'w, crate::simulation::jobs::JobBoard>,
}

#[derive(SystemParam)]
pub struct TaskDisplayParams<'w, 's> {
    pub plants: Query<'w, 's, &'static Plant>,
    pub corpse_q: Query<'w, 's, &'static Corpse>,
    pub carrying_q: Query<'w, 's, &'static crate::simulation::corpse::Carrying>,
    /// Nested so the panel's top-level param count stays under Bevy's ceiling.
    pub vehicle: VehicleInspectorParams<'w, 's>,
}

/// Vehicle inspector surface — the selected entity's `Vehicle` state plus the
/// design catalog needed to label it and derive live stats.
#[derive(SystemParam)]
pub struct VehicleInspectorParams<'w, 's> {
    pub vehicles: Query<
        'w,
        's,
        (
            &'static crate::simulation::vehicle::Vehicle,
            &'static crate::simulation::vehicle::VehicleInventory,
            &'static crate::simulation::vehicle::VehicleDraft,
            &'static crate::simulation::vehicle::VehicleCrew,
            Option<&'static crate::simulation::vehicle::VehiclePathFollow>,
            Option<&'static crate::simulation::vehicle::VehicleFireOrder>,
            Option<&'static crate::simulation::vehicle::SiegeOrder>,
            Option<&'static crate::simulation::vehicle::VehicleHealth>,
        ),
    >,
    pub registry: Res<'w, crate::simulation::vehicle::VehicleDesignRegistry>,
    pub data: Res<'w, crate::simulation::vehicle::VehicleData>,
}

/// Wage-aware-labor-market inspector surface: skill peaks, earnings,
/// per-profession EV, apprenticeship progress, perceived cross-faction
/// wages. Bundled as a SystemParam so the main panel query stays under
/// Bevy's per-tuple ceiling.
#[derive(SystemParam)]
pub struct WageInspectorParams<'w, 's> {
    pub peaks_q: Query<'w, 's, &'static crate::simulation::skills::SkillPeaks>,
    pub earnings_q: Query<'w, 's, &'static crate::simulation::jobs::Earnings>,
    pub perceived_q: Query<'w, 's, &'static crate::simulation::jobs::PerceivedFactionWages>,
    pub apprentice_q: Query<
        'w,
        's,
        (
            Option<&'static crate::simulation::apprenticeship::ApprenticeOf>,
            Option<&'static crate::simulation::apprenticeship::ApprenticeProgress>,
            Option<&'static crate::simulation::apprenticeship::MentorOf>,
        ),
    >,
    pub household_q: Query<'w, 's, &'static crate::simulation::reproduction::HouseholdMember>,
    pub disposition_q: Query<'w, 's, &'static crate::simulation::goal_scorers::Disposition>,
    pub ownership: Res<'w, crate::simulation::capital::WorkshopOwnership>,
    pub plot_q: Query<'w, 's, &'static crate::simulation::land::Plot>,
    pub plot_index: Res<'w, crate::simulation::land::PlotIndex>,
}

pub fn inspector_panel_system(
    mut contexts: EguiContexts,
    selected: Res<SelectedEntity>,
    registry: Res<FactionRegistry>,
    player_faction: Res<PlayerFaction>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    calendar: Res<Calendar>,
    sim_clock: Res<SimClock>,
    mut path_params: PathInspectorParams,
    task_display: TaskDisplayParams,
    rel_query: Query<&RelationshipMemory>,
    mut job_params: JobInspectorParams,
    wage_params: WageInspectorParams,
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
            Option<&crate::simulation::energy::Energy>,
        ),
        (
            Option<&Health>,
            Option<&Body>,
            &Transform,
            Option<&GoalReason>,
            Option<&crate::simulation::carry::Carrier>,
            Option<&crate::simulation::items::Equipment>,
            Option<&crate::simulation::knowledge::PersonKnowledge>,
            &crate::simulation::typed_task::ActionQueue,
            Option<&MethodHistory>,
            Option<&crate::simulation::player_command::Commanded>,
            Option<&crate::simulation::medicine::Injury>,
        ),
    )>,
) {
    let Some(entity) = selected.0 else { return };

    // ── Vehicle inspector ────────────────────────────────────────────────
    if let Ok((vehicle, inv, draft, crew, path, fire_order, siege_order, vhealth)) =
        task_display.vehicle.vehicles.get(entity)
    {
        use crate::simulation::vehicle::{
            derive_stats, vehicle_gunner_demand, vehicle_operator_capacity,
            VehicleDisableFlags, VehicleState,
        };
        let design = task_display.vehicle.registry.get(vehicle.design_id);
        egui::Window::new("Inspector")
            .default_pos([10.0, 10.0])
            .default_width(360.0)
            .resizable(true)
            .show(contexts.ctx_mut(), |ui| {
                egui::CollapsingHeader::new(egui::RichText::new("Vehicle").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        let name = design.map(|d| d.name.as_str()).unwrap_or("(unknown design)");
                        ui.label(format!("Design: {name}"));
                        ui.label(format!(
                            "State: {}",
                            match vehicle.state {
                                VehicleState::Parked => "Parked",
                                VehicleState::Moving => "Moving",
                                VehicleState::Loading => "Loading",
                                VehicleState::Overturned => "Overturned!",
                            }
                        ));
                        ui.label(format!(
                            "Anchor: ({}, {})  z {}  heading {}",
                            vehicle.anchor_tile.0, vehicle.anchor_tile.1, vehicle.z, vehicle.heading
                        ));
                        if let Some(design) = design {
                            if let Some((lo, hi)) = design.grid.bounds() {
                                ui.label(format!(
                                    "Footprint: {}×{}  height {}",
                                    hi.x - lo.x + 1,
                                    hi.y - lo.y + 1,
                                    hi.z - lo.z + 1
                                ));
                            }
                            let stats = derive_stats(&design.grid, &task_display.vehicle.data);
                            ui.label(format!(
                                "Empty {} kg  ·  max payload {} kg  ({} L)",
                                stats.empty_mass_g / 1000,
                                stats.max_payload_g / 1000,
                                stats.max_cargo_volume_ml / 1000,
                            ));
                            ui.label(format!(
                                "Speed: road {:.2}  off-road {:.2}",
                                stats.road_speed_cap, stats.offroad_speed_cap
                            ));
                            ui.label(format!(
                                "Stability: {:.2}  ·  turn radius {:.1}",
                                stats.stability, stats.turn_radius
                            ));
                        }
                        let carried: u32 = inv.total_qty();
                        ui.label(format!("Cargo: {carried} units"));
                        for (rid, qty) in &inv.items {
                            if *qty > 0 {
                                ui.label(format!(
                                    "  • {} ×{qty}",
                                    crate::economy::core_ids::catalog()
                                        .get(*rid)
                                        .map(|d| d.display_name.as_str())
                                        .unwrap_or("?")
                                ));
                            }
                        }
                        // Driver: prefer the explicit `VehicleCrew.driver`
                        // (player-assigned) and fall back to the cargo-haul
                        // legacy `hauler` field.
                        let driver = crew.driver.or(vehicle.hauler);
                        match driver {
                            Some(h) => ui.label(format!("Driver: entity {}", h.index())),
                            None => ui.label("Driver: none (parked)"),
                        };
                        if let Some(design) = design {
                            let capacity = vehicle_operator_capacity(
                                design,
                                &task_display.vehicle.data,
                            );
                            let demand = vehicle_gunner_demand(
                                design,
                                &task_display.vehicle.data,
                            );
                            let driver_count = if driver.is_some() { 1 } else { 0 };
                            let other_seats = capacity.saturating_sub(driver_count);
                            let passenger_cap = other_seats.saturating_sub(demand);
                            ui.label(format!(
                                "Gunners: {}/{}",
                                crew.gunners.len(),
                                demand
                            ));
                            ui.label(format!(
                                "Passengers: {}/{}",
                                crew.passengers.len(),
                                passenger_cap
                            ));
                        } else {
                            ui.label(format!("Gunners: {}", crew.gunners.len()));
                            ui.label(format!("Passengers: {}", crew.passengers.len()));
                        }
                        ui.label(format!(
                            "Draft: {}/{} animal(s) hitched",
                            draft.hitched.len(),
                            draft.required_animals
                        ));
                        match path {
                            Some(p) => ui.label(format!(
                                "Route: en route ({}/{} nodes)",
                                p.cursor,
                                p.path.len()
                            )),
                            None => ui.label("Route: idle"),
                        };
                        if let Some(order) = fire_order {
                            let remaining = order
                                .expires_tick
                                .saturating_sub(sim_clock.tick);
                            ui.label(format!(
                                "Fire order: ({}, {})  · {} ticks left",
                                order.target_tile.0, order.target_tile.1, remaining
                            ));
                        }
                        if let Some(order) = siege_order {
                            ui.label(format!(
                                "Siege order: ({}, {})",
                                order.target_tile.0, order.target_tile.1
                            ));
                        }
                        if let Some(health) = vhealth {
                            let flags = health.disabled;
                            let mut parts: Vec<&str> = Vec::new();
                            if flags.has(VehicleDisableFlags::MOVEMENT) {
                                parts.push("Mobility");
                            }
                            if flags.has(VehicleDisableFlags::STEERING) {
                                parts.push("Steering");
                            }
                            if flags.has(VehicleDisableFlags::CARGO) {
                                parts.push("Cargo");
                            }
                            if !parts.is_empty() {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 160, 60),
                                    format!("Disabled: {}", parts.join(", ")),
                                );
                            }
                        }
                        if vehicle.state == VehicleState::Overturned {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 90, 60),
                                "⚠ Overturned — movement disabled.",
                            );
                        }
                    });
            });
        return;
    }

    let Ok((
        (needs, mood, skills, ai, agent, goal, personality, sex, member, profession, stats, energy),
        (
            health,
            body,
            transform,
            goal_reason,
            carrier,
            equipment,
            knowledge,
            aq,
            method_history,
            commanded,
            injury,
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
                        let (appr_link, appr_progress, mentor_link) = wage_params
                            .apprentice_q
                            .get(entity)
                            .unwrap_or((None, None, None));
                        ui.horizontal(|ui| {
                            ui.label(format!("Profession: {:?}", profession));
                            if matches!(profession, Profession::Apprentice) {
                                if let Some(p) = appr_progress {
                                    let frac =
                                        (p.ticks as f32 / p.target_ticks.max(1) as f32).min(1.0);
                                    let days_done = p.ticks / crate::world::seasons::TICKS_PER_DAY;
                                    let total_days =
                                        p.target_ticks / crate::world::seasons::TICKS_PER_DAY;
                                    ui.separator();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "apprentice {}/{} d ({:.0}%)",
                                            days_done,
                                            total_days,
                                            frac * 100.0
                                        ))
                                        .color(egui::Color32::from_rgb(180, 200, 255))
                                        .small(),
                                    );
                                }
                            } else if let Some(m) = mentor_link {
                                ui.separator();
                                ui.label(
                                    egui::RichText::new(format!(
                                        "mentoring #{}",
                                        m.apprentice.index()
                                    ))
                                    .color(egui::Color32::from_rgb(180, 220, 180))
                                    .small(),
                                );
                            }
                        });
                        if let Some(link) = appr_link {
                            ui.label(
                                egui::RichText::new(format!("  Master: #{}", link.mentor.index()))
                                    .color(egui::Color32::from_gray(170))
                                    .small(),
                            );
                        }
                        ui.horizontal(|ui| {
                            ui.label(format!("Goal: {}", goal.name()));
                            let reason_text = goal_reason.map(|r| r.0).unwrap_or("—");
                            ui.label(
                                egui::RichText::new(format!(" ({})", reason_text))
                                    .small()
                                    .color(egui::Color32::from_gray(160)),
                            );
                        });
                        // Heal-6: injury readout. Severity = 0..=255
                        // (255 = fully wrecked); colour goes from yellow
                        // (light wound) to red (severe) so the operator
                        // can scan a settlement and spot triage cases.
                        // `last_damage_tick` lets the operator see how
                        // recent the injury is — a stale `applied_tick`
                        // with old `last_damage_tick` is recovering, a
                        // fresh `last_damage_tick` is still combat-hot.
                        if let Some(inj) = injury {
                            let sev = inj.severity as f32 / 255.0;
                            let red = (180.0 + 75.0 * sev).clamp(180.0, 255.0) as u8;
                            let green = (180.0 - 150.0 * sev).clamp(30.0, 180.0) as u8;
                            let age_ticks = sim_clock.tick.saturating_sub(inj.applied_tick);
                            let age_days = age_ticks / crate::world::seasons::TICKS_PER_DAY as u64;
                            let since_damage = sim_clock.tick.saturating_sub(inj.last_damage_tick);
                            let recency = if since_damage
                                < (crate::world::seasons::TICKS_PER_DAY / 2) as u64
                            {
                                "fresh"
                            } else {
                                "stable"
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "Injured: severity {} ({}, {} d since onset)",
                                    inj.severity, recency, age_days,
                                ))
                                .color(egui::Color32::from_rgb(red, green, 40)),
                            );
                        }
                        if let Ok(claim) = job_params.claim_query.get(entity) {
                            ui.horizontal(|ui| {
                                ui.label(format!("Job: {} (#{})", claim.kind.name(), claim.job_id));
                                if ui.button("Release").clicked() {
                                    job_params.commands.send(
                                        crate::simulation::jobs::JobBoardCommand::Cancel(
                                            claim.job_id,
                                        ),
                                    );
                                }
                            });
                            // Extra job details: source, progress, fail count
                            if let Some(postings) = job_params.board.postings.get(&claim.faction_id)
                            {
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
                                            .on_hover_text(
                                                "Number of times worker got stuck or timed out",
                                            );
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
                        needs_bar(ui, "Thirst", needs.thirst);
                        needs_bar(ui, "Sleep", needs.sleep);
                        needs_bar(ui, "Shelter", needs.shelter);
                        needs_bar(ui, "Safety", needs.safety);
                        needs_bar(ui, "Social", needs.social);
                        needs_bar(ui, "Repro", needs.reproduction);
                        willpower_bar(ui, needs.willpower);
                        if let Some(e) = energy {
                            let label = if e.is_exhausted() {
                                "Energy (EXHAUSTED)"
                            } else if e.is_tired() {
                                "Energy (tired)"
                            } else {
                                "Energy"
                            };
                            ui.label(format!(
                                "{}: {:.0} / {:.0}",
                                label, e.current, e.max
                            ));
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Skills").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        let peaks = wage_params.peaks_q.get(entity).ok().copied();
                        for i in 0..SKILL_COUNT {
                            let kind = SkillKind::ALL[i];
                            let cur = skills.0[i];
                            let (peak, floor) = match peaks {
                                Some(p) => {
                                    let pk = p.0[i];
                                    (pk, crate::simulation::skills::skill_floor(pk))
                                }
                                None => (cur, cur),
                            };
                            let mastered = peak >= crate::simulation::skills::SKILL_MASTERY_LINE;
                            let mut line = egui::RichText::new(format!(
                                "  {}: {} / peak {} (floor {})",
                                kind.name(),
                                cur,
                                peak,
                                floor
                            ));
                            if mastered {
                                line = line.color(egui::Color32::from_rgb(180, 220, 180));
                            }
                            ui.label(line);
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
                egui::CollapsingHeader::new(egui::RichText::new("Wage & Labor").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        wage_labor_section(
                            ui,
                            entity,
                            agent,
                            carrier,
                            transform,
                            skills,
                            member,
                            profession,
                            &registry,
                            &sim_clock,
                            &wage_params,
                        );
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Knowledge").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(k) = knowledge {
                            let used = k.complexity_used();
                            let learned_count = (0..crate::simulation::technology::TECH_COUNT
                                as crate::simulation::technology::TechId)
                                .filter(|id| k.has_learned(*id))
                                .count();
                            let speed = stats
                                .map(|s| {
                                    1.0 / crate::simulation::knowledge::learning_slowdown(s, k)
                                })
                                .unwrap_or(1.0);
                            ui.label(format!(
                                "Knowledge: {} complexity pts ({} techs)",
                                used, learned_count
                            ));
                            ui.label(format!("Learning speed: {:.2}×", speed));
                            let mut learned: Vec<crate::simulation::technology::TechId> =
                                Vec::new();
                            let mut aware_only: Vec<crate::simulation::technology::TechId> =
                                Vec::new();
                            for id in 0..crate::simulation::technology::TECH_COUNT
                                as crate::simulation::technology::TechId
                            {
                                if k.has_learned(id) {
                                    learned.push(id);
                                } else if k.is_aware(id) {
                                    aware_only.push(id);
                                }
                            }
                            ui.label(format!("Learned ({}):", learned.len()));
                            egui::ScrollArea::vertical()
                                .id_salt("knowledge_learned")
                                .max_height(140.0)
                                .show(ui, |ui| {
                                    for id in &learned {
                                        let def = crate::simulation::technology::tech_def(*id);
                                        let cx = crate::simulation::technology::complexity(*id);
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "  \u{2713} {} ({}, {} pts)",
                                                def.name,
                                                def.era.name(),
                                                cx
                                            ));
                                            if ui
                                                .small_button("Lecture")
                                                .on_hover_text(
                                                    "Have this person hold a lecture on this tech",
                                                )
                                                .clicked()
                                            {
                                                path_params.pending_action.0 =
                                                    Some(InspectorActionKind::HoldLecture {
                                                        lecturer: entity,
                                                        tech: *id,
                                                    });
                                            }
                                            if ui.small_button("Encode").on_hover_text(
                                                "Faction crafts a Clay Tablet encoding this tech"
                                            ).clicked() {
                                                path_params.pending_action.0 =
                                                    Some(InspectorActionKind::EncodeTablet {
                                                        tech: *id,
                                                    });
                                            }
                                        });
                                    }
                                    if learned.is_empty() {
                                        ui.label("  (none)");
                                    }
                                });
                            // In-flight study progress (tracked per tech).
                            if !k.study_progress.is_empty() {
                                ui.separator();
                                ui.label("In progress:");
                                let mut entries: Vec<(crate::simulation::technology::TechId, u32)> =
                                    k.study_progress.iter().map(|(t, p)| (*t, *p)).collect();
                                entries.sort_by_key(|e| e.0);
                                for (tech, prog) in entries {
                                    let def = crate::simulation::technology::tech_def(tech);
                                    let thr = crate::simulation::knowledge::study_threshold(tech);
                                    ui.label(format!("  · {}: {}/{} ticks", def.name, prog, thr));
                                }
                            }
                            ui.label(format!("Aware of ({}):", aware_only.len()));
                            egui::ScrollArea::vertical()
                                .id_salt("knowledge_aware")
                                .max_height(140.0)
                                .show(ui, |ui| {
                                    // Determine which aware-only techs the
                                    // agent has a readable tablet/book for.
                                    let mut readable: ahash::AHashSet<
                                        crate::simulation::technology::TechId,
                                    > = ahash::AHashSet::new();
                                    for (item, qty) in agent.inventory.iter() {
                                        if *qty == 0 {
                                            continue;
                                        }
                                        let tablet_id =
                                            crate::economy::core_ids::ClayTablet.get().copied();
                                        let book_id = crate::economy::core_ids::Book.get().copied();
                                        let rid = item.resource_id;
                                        if Some(rid) != tablet_id && Some(rid) != book_id {
                                            continue;
                                        }
                                        if let Some(t) = item.tech_payload {
                                            readable.insert(t);
                                        }
                                    }
                                    for id in &aware_only {
                                        let def = crate::simulation::technology::tech_def(*id);
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "  \u{25CE} {} ({})",
                                                def.name,
                                                def.era.name()
                                            ));
                                            if readable.contains(id) {
                                                if ui.small_button("Read").clicked() {
                                                    path_params.pending_action.0 =
                                                        Some(InspectorActionKind::ReadItem {
                                                            reader: entity,
                                                            tech: *id,
                                                        });
                                                }
                                            }
                                        });
                                    }
                                    if aware_only.is_empty() {
                                        ui.label("  (none)");
                                    }
                                });
                            // Phase I — Beliefs subsection. Lists every
                            // accepted belief per group with its confidence
                            // bar + truth-status colour cue. Pulls from
                            // `PersonKnowledge.belief` (populated by Phase
                            // H's `seed_initial_beliefs` and any in-game
                            // belief swaps).
                            ui.separator();
                            ui.label(egui::RichText::new("Beliefs").strong());
                            if k.belief.is_empty() {
                                ui.label("  (no beliefs held)");
                            } else {
                                use crate::simulation::knowledge_catalog::{
                                    knowledge_def, TruthStatus,
                                };
                                let mut groups: Vec<(u8, &crate::simulation::knowledge::BeliefState)> =
                                    k.belief.iter().map(|(g, s)| (*g, s)).collect();
                                groups.sort_by_key(|(g, _)| *g);
                                for (group, state) in groups {
                                    let def = knowledge_def(state.accepted);
                                    let group_label = match group {
                                        crate::simulation::knowledge_catalog::BELIEF_GROUP_COSMOLOGY => "Cosmology",
                                        crate::simulation::knowledge_catalog::BELIEF_GROUP_DISEASE_CAUSATION => "Disease",
                                        crate::simulation::knowledge_catalog::BELIEF_GROUP_OMENS => "Omens",
                                        _ => "Other",
                                    };
                                    let truth_colour = match def.truth() {
                                        TruthStatus::True => egui::Color32::from_rgb(120, 200, 120),
                                        TruthStatus::FalseUseful => egui::Color32::from_rgb(220, 200, 100),
                                        TruthStatus::FalseHarmful => egui::Color32::from_rgb(220, 120, 100),
                                        TruthStatus::Contested => egui::Color32::from_rgb(180, 180, 220),
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(format!("  {}:", group_label));
                                        ui.label(
                                            egui::RichText::new(def.name()).color(truth_colour),
                                        );
                                        ui.add(
                                            egui::ProgressBar::new(
                                                state.confidence as f32 / 255.0,
                                            )
                                            .desired_width(70.0)
                                            .text(format!("{}%", state.confidence as u32 * 100 / 255)),
                                        );
                                    });
                                    if state.rejected_len > 0 {
                                        let names: Vec<&'static str> = state
                                            .rejected_iter()
                                            .map(|id| knowledge_def(id).name())
                                            .collect();
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "    rejected: {}",
                                                names.join(", ")
                                            ))
                                            .color(egui::Color32::GRAY)
                                            .size(11.0),
                                        );
                                    }
                                }
                            }
                        } else {
                            ui.label("  (no knowledge component)");
                        }
                    });
                egui::CollapsingHeader::new(egui::RichText::new("Currency & Inventory").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(format!("Currency: {:.1}", agent.currency));

                        let cur_g = agent.current_weight_g();
                        let cap_g = agent.capacity_g();
                        let small_cur = agent.current_small_vol_ml();
                        let small_cap = agent.capacity_small_vol_ml();
                        let bulky_cur = agent.current_bulky_vol_ml();
                        let bulky_cap = agent.capacity_bulky_vol_ml();
                        ui.label(format!(
                            "Inventory: {:.1} / {:.1} kg  |  pouch {:.1}/{:.1} L  |  pack {:.1}/{:.1} L",
                            cur_g as f32 / 1000.0,
                            cap_g as f32 / 1000.0,
                            small_cur as f32 / 1000.0,
                            small_cap as f32 / 1000.0,
                            bulky_cur as f32 / 1000.0,
                            bulky_cap as f32 / 1000.0,
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
                            .max_height(120.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (item, qty) in &agent.inventory {
                                    if *qty > 0 {
                                        ui.horizontal(|ui| {
                                            ui.label(format!(
                                                "{}: {} ({:.2} kg)",
                                                item.label(),
                                                qty,
                                                item.stack_weight_g(*qty) as f32 / 1000.0
                                            ));
                                            if ui.small_button("Drop 1").clicked() {
                                                path_params.pending_action.0 =
                                                    Some(InspectorActionKind::DropInvItemOne {
                                                        target: entity,
                                                        item: *item,
                                                    });
                                            }
                                            if *qty > 1 && ui.small_button("Drop All").clicked() {
                                                path_params.pending_action.0 =
                                                    Some(InspectorActionKind::DropInventoryItem {
                                                        target: entity,
                                                        item: *item,
                                                        qty: *qty,
                                                    });
                                            }
                                        });
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
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "  L: {} ×{}{} ({:.2} kg)",
                                        stack.item.label(),
                                        stack.qty,
                                        tag,
                                        stack.weight_g() as f32 / 1000.0,
                                    ));
                                    if ui.small_button("Drop").clicked() {
                                        path_params.pending_action.0 =
                                            Some(InspectorActionKind::DropLeftHand {
                                                target: entity,
                                            });
                                    }
                                });
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
                                    ui.horizontal(|ui| {
                                        ui.label(format!(
                                            "  R: {} ×{}{} ({:.2} kg)",
                                            stack.item.label(),
                                            stack.qty,
                                            tag,
                                            stack.weight_g() as f32 / 1000.0,
                                        ));
                                        if ui.small_button("Drop").clicked() {
                                            path_params.pending_action.0 =
                                                Some(InspectorActionKind::DropRightHand {
                                                    target: entity,
                                                });
                                        }
                                    });
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

                egui::CollapsingHeader::new(egui::RichText::new("Equipment").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        if let Some(equip) = equipment {
                            const SLOTS: &[(EquipmentSlot, &str)] = &[
                                (EquipmentSlot::MainHand, "Main Hand"),
                                (EquipmentSlot::OffHand, "Off Hand"),
                                (EquipmentSlot::TorsoArmor, "Torso"),
                                (EquipmentSlot::HeadArmor, "Head"),
                                (EquipmentSlot::ArmArmor, "Arms"),
                                (EquipmentSlot::LegArmor, "Legs"),
                            ];
                            for &(slot, slot_name) in SLOTS {
                                if let Some(item) = equip.items.get(&slot) {
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{slot_name}: {}", item.label()));
                                        if ui.small_button("Unequip").clicked() {
                                            path_params.pending_action.0 =
                                                Some(InspectorActionKind::UnequipSlot {
                                                    target: entity,
                                                    slot,
                                                });
                                        }
                                    });
                                } else {
                                    ui.label(
                                        egui::RichText::new(format!("{slot_name}: —"))
                                            .color(egui::Color32::from_gray(140)),
                                    );
                                }
                            }

                            // Equip buttons for equippable items in inventory or hands
                            let mut has_equippable = false;
                            for (item, qty) in &agent.inventory {
                                if *qty > 0 && !valid_equip_slots(item.resource_id).is_empty() {
                                    has_equippable = true;
                                    break;
                                }
                            }
                            if !has_equippable {
                                if let Some(c) = carrier {
                                    for stack in [c.left, c.right].into_iter().flatten() {
                                        if !valid_equip_slots(stack.item.resource_id).is_empty() {
                                            has_equippable = true;
                                            break;
                                        }
                                    }
                                }
                            }

                            if has_equippable {
                                ui.separator();
                                ui.label(
                                    egui::RichText::new("Equip from inventory/hands:")
                                        .color(egui::Color32::from_gray(180))
                                        .small(),
                                );
                                for (item, qty) in &agent.inventory {
                                    if *qty == 0 {
                                        continue;
                                    }
                                    for &slot in valid_equip_slots(item.resource_id) {
                                        let slot_name = slot_display_name(slot);
                                        if ui
                                            .small_button(format!("{} → {slot_name}", item.label()))
                                            .clicked()
                                        {
                                            path_params.pending_action.0 =
                                                Some(InspectorActionKind::EquipItem {
                                                    target: entity,
                                                    item: *item,
                                                    from_hands: false,
                                                    slot,
                                                });
                                        }
                                    }
                                }
                                if let Some(c) = carrier {
                                    for (stack, from_left) in [(c.left, true), (c.right, false)] {
                                        let Some(stack) = stack else { continue };
                                        for &slot in valid_equip_slots(stack.item.resource_id) {
                                            let slot_name = slot_display_name(slot);
                                            let hand_tag = if stack.two_handed {
                                                "hands"
                                            } else if from_left {
                                                "L"
                                            } else {
                                                "R"
                                            };
                                            if ui
                                                .small_button(format!(
                                                    "{} ({hand_tag}) → {slot_name}",
                                                    stack.item.label()
                                                ))
                                                .clicked()
                                            {
                                                path_params.pending_action.0 =
                                                    Some(InspectorActionKind::EquipItem {
                                                        target: entity,
                                                        item: stack.item,
                                                        from_hands: true,
                                                        slot,
                                                    });
                                            }
                                            if stack.two_handed {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("(no equipment component)")
                                    .color(egui::Color32::from_gray(140)),
                            );
                        }
                    });

                egui::CollapsingHeader::new(egui::RichText::new("Task & State").strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        let task_name = task_kind_label(aq.current_task_kind());
                        ui.label(format!("Task: {}", task_name));
                        if let Some(c) = commanded {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Commanded: {:?} → {:?}  (#{} @ tick {})",
                                    c.command, c.status, c.command_id, c.issued_tick
                                ))
                                .color(egui::Color32::from_rgb(110, 200, 240)),
                            );
                        }
                        use crate::simulation::typed_task::Task;
                        let method_label = match ai.active_method {
                            Some(m) => m.name().to_string(),
                            None if aq.current != Task::Idle => {
                                "(direct dispatch — no HTN method)".to_string()
                            }
                            None => "(none — see HTN history)".to_string(),
                        };
                        ui.label(format!("Method: {}", method_label));

                        let now = sim_clock.tick;
                        if aq.current_task_kind() == UNEMPLOYED_TASK_KIND
                            && aq.current == Task::Idle
                        {
                            let last_attempt = method_history
                                .and_then(|h| {
                                    h.entries
                                        .iter()
                                        .filter_map(|e| *e)
                                        .max_by_key(|(_, _, t)| *t)
                                })
                                .filter(|(_, _, tick)| {
                                    now.saturating_sub(*tick) <= METHOD_HISTORY_TTL_TICKS
                                });
                            let attempt_str = match last_attempt {
                                Some((mid, outcome, tick)) => format!(
                                    "last attempt: {} → {:?} ({}t ago)",
                                    mid.name(),
                                    outcome,
                                    now.saturating_sub(tick)
                                ),
                                None => "last attempt: none (no method dispatched in last 30s)"
                                    .to_string(),
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "Idle: ticks_idle={}  •  last_goal_eval={}  •  {}",
                                    ai.ticks_idle, ai.last_goal_eval_tick, attempt_str
                                ))
                                .color(egui::Color32::from_rgb(220, 140, 120)),
                            );
                        }

                        if let Some(history) = method_history {
                            let mut live: Vec<(MethodOutcome, &'static str, u64)> = history
                                .entries
                                .iter()
                                .filter_map(|e| *e)
                                .filter(|(_, _, t)| {
                                    now.saturating_sub(*t) <= METHOD_HISTORY_TTL_TICKS
                                })
                                .map(|(mid, outcome, t)| (outcome, mid.name(), t))
                                .collect();
                            live.sort_by(|a, b| b.2.cmp(&a.2));
                            if !live.is_empty() {
                                egui::CollapsingHeader::new("HTN history")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for (outcome, name, tick) in &live {
                                            let color = match outcome {
                                                MethodOutcome::Success => {
                                                    egui::Color32::from_rgb(140, 200, 140)
                                                }
                                                MethodOutcome::FailedRouting
                                                | MethodOutcome::FailedTarget => {
                                                    egui::Color32::from_rgb(220, 110, 110)
                                                }
                                                MethodOutcome::Interrupted => {
                                                    egui::Color32::from_gray(160)
                                                }
                                                MethodOutcome::Abandoned => {
                                                    egui::Color32::from_rgb(200, 150, 90)
                                                }
                                            };
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{}  {:?}  ({}t ago)",
                                                    name,
                                                    outcome,
                                                    now.saturating_sub(*tick)
                                                ))
                                                .color(color),
                                            );
                                        }
                                    });
                            }
                        }

                        let state_desc = match ai.state() {
                            AiState::Idle => "Idling".to_string(),
                            AiState::Routing => "Traveling (Long Range)".to_string(),
                            AiState::Seeking => "Walking to Target".to_string(),
                            AiState::Sleeping => "Sleeping".to_string(),
                            AiState::Attacking => "In Combat".to_string(),
                            AiState::Working => {
                                let tx = ai.target_tile.0 as i32;
                                let ty = ai.target_tile.1 as i32;
                                let mut work_str = "Working".to_string();

                                if aq.current_task_kind() == TaskKind::Gather as u16 {
                                    if let Some(&p_entity) = plant_map.0.get(&(tx, ty)) {
                                        if let Ok(plant) = task_display.plants.get(p_entity) {
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
                                } else if aq.current_task_kind() == TaskKind::Planter as u16 {
                                    work_str = format!(
                                        "Planting Seeds ({}%)",
                                        (ai.work_progress as u32 * 100) / 40
                                    );
                                // 40 is TICKS_FARMER_PLANT
                                } else if aq.current_task_kind() == TaskKind::Raid as u16 {
                                    work_str = "Stealing Goods".to_string();
                                } else if aq.current_task_kind() == TaskKind::Scavenge as u16 {
                                    work_str = "Picking up item".to_string();
                                } else if aq.current_task_kind()
                                    == TaskKind::WithdrawMaterial as u16
                                {
                                    if let Some((rid, qty)) = aq.current.as_withdraw_material() {
                                        work_str = format!(
                                            "Withdrawing {} \u{00d7} {}",
                                            crate::economy::core_ids::display_name(rid),
                                            qty
                                        );
                                    } else {
                                        work_str = "Withdrawing".to_string();
                                    }
                                } else if aq.current_task_kind() == TaskKind::Butcher as u16 {
                                    work_str = format!(
                                        "Butchering ({}%)",
                                        (ai.work_progress as u32 * 100) / 60
                                    );
                                } else if aq.current_task_kind()
                                    == TaskKind::WorkOnCraftOrder as u16
                                {
                                    work_str = format!("Crafting (step: {})", ai.work_progress);
                                } else if aq.current_task_kind() == TaskKind::WithdrawGood as u16 {
                                    use crate::simulation::typed_task::WithdrawGoodFilter;
                                    let good_label = match aq.current.as_withdraw_good() {
                                        Some(WithdrawGoodFilter::AnyEntertainment) => {
                                            "entertainment good".to_owned()
                                        }
                                        Some(WithdrawGoodFilter::Specific(rid)) => {
                                            crate::economy::core_ids::display_name(rid).to_owned()
                                        }
                                        None => "good".to_owned(),
                                    };
                                    work_str = format!("Withdrawing {}", good_label);
                                } else if aq.current_task_kind() == TaskKind::PlayPlant as u16 {
                                    work_str = format!(
                                        "Play-Planting ({}%)",
                                        (ai.work_progress as u32 * 100) / 40
                                    );
                                } else if aq.current_task_kind() == TaskKind::PlayThrow as u16 {
                                    work_str = format!(
                                        "Play-Throwing ({}%)",
                                        (ai.work_progress as u32 * 100) / 30
                                    );
                                }

                                work_str
                            }
                        };
                        ui.label(format!("State: {}", state_desc));
                        ui.label(format!(
                            "Target: {}, {}",
                            ai.target_tile.0, ai.target_tile.1
                        ));
                        if let Some((rid, qty)) = aq.current.as_withdraw_material() {
                            ui.label(format!(
                                "Withdraw intent: {} \u{00d7} {} from ({}, {})",
                                crate::economy::core_ids::display_name(rid),
                                qty,
                                ai.dest_tile.0,
                                ai.dest_tile.1
                            ));
                        }
                        if let Ok(carrying) = task_display.carrying_q.get(entity) {
                            if let Ok(corpse) = task_display.corpse_q.get(carrying.0) {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Carrying: {:?} Corpse",
                                        corpse.species
                                    ))
                                    .color(egui::Color32::from_rgb(180, 120, 80)),
                                );
                            }
                        }

                        match commanded {
                            Some(c) => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Order: {:?} (#{} — {:?})",
                                        c.command, c.command_id, c.status
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
                            .filter(|(x, y, _)| !(*x == i32::MIN && *y == i32::MIN))
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
                                pf.recent_tiles = [(i32::MIN, i32::MIN, 0); 4];
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

fn slot_display_name(slot: EquipmentSlot) -> &'static str {
    match slot {
        EquipmentSlot::MainHand => "Main Hand",
        EquipmentSlot::OffHand => "Off Hand",
        EquipmentSlot::TorsoArmor => "Torso",
        EquipmentSlot::HeadArmor => "Head",
        EquipmentSlot::ArmArmor => "Arms",
        EquipmentSlot::LegArmor => "Legs",
    }
}

/// Executes pending inspector inventory/equipment actions queued by
/// `inspector_panel_system`. Runs in Update, after `inspector_panel_system`.
pub fn inspector_action_system(
    mut commands: Commands,
    spatial: Res<SpatialIndex>,
    mut pending: ResMut<PendingInspectorAction>,
    mut sender: crate::simulation::player_command::CommandSender,
    mut worker_q: Query<(&Transform, &mut EconomicAgent, &mut Carrier, &mut Equipment)>,
    mut ground_items: Query<&mut GroundItem>,
) {
    let Some(action) = pending.0.take() else {
        return;
    };

    match action {
        InspectorActionKind::DropInventoryItem { target, item, qty } => {
            let Ok((transform, mut agent, _, _)) = worker_q.get_mut(target) else {
                return;
            };
            let removed = agent.remove_item(item, qty);
            if removed > 0 {
                let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    item,
                    removed,
                );
            }
        }
        InspectorActionKind::DropInvItemOne { target, item } => {
            let Ok((transform, mut agent, _, _)) = worker_q.get_mut(target) else {
                return;
            };
            let removed = agent.remove_item(item, 1);
            if removed > 0 {
                let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    item,
                    1,
                );
            }
        }
        InspectorActionKind::DropLeftHand { target } => {
            let Ok((transform, mut agent, mut carrier, _)) = worker_q.get_mut(target) else {
                return;
            };
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            if let Some(stack) = carrier.left.take() {
                if stack.two_handed {
                    carrier.right = None;
                }
                spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    stack.item,
                    stack.qty,
                );
                // Drop-hand may free capacity for bonuses — recompute via agent
                let _ = agent.bonus_cap_g; // touch to avoid unused warning
            }
        }
        InspectorActionKind::DropRightHand { target } => {
            let Ok((transform, mut agent, mut carrier, _)) = worker_q.get_mut(target) else {
                return;
            };
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            // Only drop right if not already cleared by a two-handed left drop.
            if let Some(stack) = carrier.right.take() {
                spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    stack.item,
                    stack.qty,
                );
                let _ = agent.bonus_cap_g;
            }
        }
        InspectorActionKind::EquipItem {
            target,
            item,
            from_hands,
            slot,
        } => {
            let Ok((transform, mut agent, mut carrier, mut equipment)) = worker_q.get_mut(target)
            else {
                return;
            };
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

            // Remove from source
            let found = if from_hands {
                carrier.remove_item(item, 1) > 0
            } else {
                agent.remove_item(item, 1) > 0
            };
            if !found {
                return;
            }

            // Insert into slot, handle displaced item
            let displaced = equipment.items.insert(slot, item);
            if let Some(prev) = displaced {
                let leftover = agent.add_item(prev, 1);
                if leftover > 0 {
                    let leftover2 = carrier.try_pick_up(prev, leftover);
                    if leftover2 > 0 {
                        spawn_or_merge_ground_item_full(
                            &mut commands,
                            &spatial,
                            &mut ground_items,
                            tx,
                            ty,
                            prev,
                            leftover2,
                        );
                    }
                }
            }
        }
        InspectorActionKind::UnequipSlot { target, slot } => {
            let Ok((transform, mut agent, mut carrier, mut equipment)) = worker_q.get_mut(target)
            else {
                return;
            };
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;

            let Some(item) = equipment.items.remove(&slot) else {
                return;
            };
            let leftover = agent.add_item(item, 1);
            if leftover > 0 {
                let leftover2 = carrier.try_pick_up(item, leftover);
                if leftover2 > 0 {
                    spawn_or_merge_ground_item_full(
                        &mut commands,
                        &spatial,
                        &mut ground_items,
                        tx,
                        ty,
                        item,
                        leftover2,
                    );
                }
            }
        }
        InspectorActionKind::HoldLecture { lecturer, tech } => {
            sender.send(
                vec![lecturer],
                crate::simulation::player_command::PlayerCommand::HoldLecture { tech },
            );
        }
        InspectorActionKind::ReadItem { reader, tech } => {
            sender.send(
                vec![reader],
                crate::simulation::player_command::PlayerCommand::ReadItem { tech },
            );
        }
        InspectorActionKind::EncodeTablet { tech } => {
            // EncodeTablet is faction-level — `CommandSender` fills in
            // the sender's faction id from `PlayerFaction`, and the
            // payload carries it forward to the drain.
            let _ = spatial;
            let fid = sender.player_faction.faction_id;
            sender.send(
                vec![],
                crate::simulation::player_command::PlayerCommand::EncodeTablet {
                    tech,
                    faction_id: fid,
                },
            );
        }
    }
}

/// Render the "Wage & Labor" inspector section (Phase 6 of
/// wage-aware-labor-market-v2). Surfaces:
/// - Earnings ring summary (count + total in window)
/// - Per-profession EV table — `expected_wage = wage × competence × capital`
/// - Own-faction `wage_signal` rows by EMA per game-day
/// - Cross-faction perceived wages from gossip
#[allow(clippy::too_many_arguments)]
fn wage_labor_section(
    ui: &mut egui::Ui,
    entity: Entity,
    agent: &EconomicAgent,
    carrier: Option<&Carrier>,
    transform: &Transform,
    skills: &Skills,
    member: &FactionMember,
    profession: &Profession,
    registry: &FactionRegistry,
    sim_clock: &SimClock,
    wage_params: &WageInspectorParams,
) {
    use crate::simulation::profession_choice::{
        aggregate_wage_per_day, expected_wage, primary_skill_for,
    };

    let now = sim_clock.tick as u32;
    let day = crate::world::seasons::TICKS_PER_DAY;
    let window_start = now.saturating_sub(day);

    // --- Earnings -----------------------------------------------------
    if let Ok(earn) = wage_params.earnings_q.get(entity) {
        let mut day_total = 0.0_f32;
        let mut day_count = 0u32;
        let mut ring_total = 0.0_f32;
        for e in earn.recent.iter() {
            ring_total += e.amount;
            if e.tick >= window_start {
                day_total += e.amount;
                day_count += 1;
            }
        }
        ui.label(format!(
            "Earnings (24h): {:.1} ({} payouts) • ring: {:.1}",
            day_total, day_count, ring_total
        ));
        if !earn.recent.is_empty() {
            let last = earn.recent.back().copied().unwrap();
            let rid_str = last
                .target_rid
                .and_then(|r| {
                    crate::economy::core_ids::catalog()
                        .get(r)
                        .map(|d| d.display_name.clone())
                })
                .unwrap_or_else(|| "—".to_string());
            ui.label(
                egui::RichText::new(format!(
                    "  last: {} {} +{:.2} @{}",
                    last.job_kind.name(),
                    rid_str,
                    last.amount,
                    last.tick
                ))
                .small()
                .color(egui::Color32::from_gray(170)),
            );
        }
    } else {
        ui.label(
            egui::RichText::new("Earnings: (no ring component)")
                .small()
                .color(egui::Color32::from_gray(140)),
        );
    }

    // --- Disposition ---------------------------------------------------
    let disposition = wage_params
        .disposition_q
        .get(entity)
        .copied()
        .unwrap_or_default();
    ui.separator();
    ui.label(
        egui::RichText::new(format!(
            "Disposition: ent {} • greg {} • cur {} • mar {}",
            disposition.entrepreneurial,
            disposition.gregariousness,
            disposition.curiosity,
            disposition.martial
        ))
        .small()
        .color(egui::Color32::from_gray(170)),
    );

    // --- Per-profession EV table -------------------------------------
    let village_id = registry.root_faction(member.faction_id);
    let faction = registry.factions.get(&village_id);
    if member.faction_id != SOLO {
        if let Some(faction) = faction {
            ui.separator();
            ui.label(
                egui::RichText::new(format!(
                    "Expected wage per day (EarnIncome ×{:.2})",
                    disposition.earn_income_multiplier()
                ))
                .strong()
                .small(),
            );
            let agent_tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            let household = wage_params.household_q.get(entity).ok();
            let candidates = [
                Profession::Farmer,
                Profession::Hunter,
                Profession::Crafter,
                Profession::Bureaucrat,
                Profession::Trader,
                Profession::Healer,
            ];
            for prof in candidates {
                let cap = match carrier {
                    Some(c) => crate::simulation::capital::capital_factor(
                        agent,
                        c,
                        agent_tile,
                        village_id,
                        household,
                        prof,
                        &wage_params.ownership,
                        &wage_params.plot_q,
                        &wage_params.plot_index,
                    ),
                    None => 1.0,
                };
                let ev = expected_wage(faction, prof, skills, cap);
                let agg = aggregate_wage_per_day(faction, prof);
                let comp = primary_skill_for(prof)
                    .map(|k| crate::simulation::profession_choice::skill_competence(skills.get(k)))
                    .unwrap_or(1.0);
                let mut text = egui::RichText::new(format!(
                    "  {:?}: EV {:.2} (wage {:.2} × comp {:.2} × cap {:.2})",
                    prof, ev, agg, comp, cap
                ))
                .small();
                if *profession == prof {
                    text = text.color(egui::Color32::from_rgb(200, 220, 120));
                } else if ev <= 0.0 {
                    text = text.color(egui::Color32::from_gray(130));
                }
                ui.label(text);
            }
        }
    }

    // --- Own-faction wage_signal --------------------------------------
    if let Some(faction) = faction {
        if !faction.wage_signal.is_empty() {
            ui.separator();
            ui.label(
                egui::RichText::new(format!("Faction #{} wage signal", village_id))
                    .strong()
                    .small(),
            );
            let mut rows: Vec<_> = faction.wage_signal.iter().collect();
            rows.sort_by(|a, b| {
                b.1.ema_per_day
                    .partial_cmp(&a.1.ema_per_day)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for ((kind, rid), ema) in rows.into_iter().take(6) {
                let rid_str = rid
                    .and_then(|r| {
                        crate::economy::core_ids::catalog()
                            .get(r)
                            .map(|d| d.display_name.clone())
                    })
                    .unwrap_or_else(|| "—".to_string());
                ui.label(
                    egui::RichText::new(format!(
                        "  {:?}/{}: {:.2}/day (n={})",
                        kind, rid_str, ema.ema_per_day, ema.samples
                    ))
                    .small(),
                );
            }
        } else if member.faction_id != SOLO {
            ui.label(
                egui::RichText::new("Faction wage signal: (no payouts yet)")
                    .small()
                    .color(egui::Color32::from_gray(140)),
            );
        }
    }

    // --- Perceived cross-faction wages (gossip) ----------------------
    if let Ok(perceived) = wage_params.perceived_q.get(entity) {
        if !perceived.by_key.is_empty() {
            ui.separator();
            ui.label(
                egui::RichText::new("Perceived wages (gossip)")
                    .strong()
                    .small(),
            );
            let mut rows: Vec<_> = perceived.by_key.iter().collect();
            rows.sort_by(|a, b| {
                b.1 .0
                    .partial_cmp(&a.1 .0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for ((fid, kind, rid), (ema, observed)) in rows.into_iter().take(6) {
                let rid_str = rid
                    .and_then(|r| {
                        crate::economy::core_ids::catalog()
                            .get(r)
                            .map(|d| d.display_name.clone())
                    })
                    .unwrap_or_else(|| "—".to_string());
                let age = now.saturating_sub(*observed);
                let days_old = age as f32 / day as f32;
                ui.label(
                    egui::RichText::new(format!(
                        "  #{} {:?}/{}: {:.2}/day ({:.1}d old)",
                        fid, kind, rid_str, ema, days_old
                    ))
                    .small(),
                );
            }
        }
    }
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
