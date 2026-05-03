use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::goods::Good;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

use super::faction::{FactionMember, FactionRegistry, HuntOrder, HUNT_PARTY_TIMEOUT};
use super::items::{spawn_or_merge_ground_item, GroundItem};
use super::lod::LodLevel;
use super::person::{AiState, PersonAI};
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use super::tasks::TaskKind;

/// Species of a `Corpse`. Only includes huntable animals (Wolf, Deer); other
/// animal species despawn cleanly without leaving a corpse, matching their
/// pre-overhaul "no drops" behavior.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorpseSpecies {
    Wolf,
    Deer,
}

impl CorpseSpecies {
    pub fn label(self) -> &'static str {
        match self {
            CorpseSpecies::Wolf => "Wolf",
            CorpseSpecies::Deer => "Deer",
        }
    }
}

/// A dead animal awaiting butchery. Created by `death_system` for hunted
/// species. Lives until `fresh_until_tick`, then `corpse_decay_system`
/// despawns it (rotted away — no salvage).
#[derive(Component, Clone, Copy)]
pub struct Corpse {
    pub species: CorpseSpecies,
    pub fresh_until_tick: u64,
}

/// Freshness window in ticks (~30 s of game time at 20 Hz). A hunter has
/// this long to drag the corpse to a butcher site before it rots.
pub const CORPSE_FRESHNESS_TICKS: u64 = 600;

/// Per-tile index of `Corpse` entities. Updated by `death_system` (insert)
/// and `corpse_decay_system` / Butcher executor (remove). Used by the
/// `NearestFreshCorpse` step resolver.
#[derive(Resource, Default)]
pub struct CorpseMap(pub AHashMap<(i32, i32), Vec<Entity>>);

impl CorpseMap {
    pub fn insert(&mut self, tile: (i32, i32), e: Entity) {
        self.0.entry(tile).or_default().push(e);
    }

    pub fn remove(&mut self, tile: (i32, i32), e: Entity) {
        if let Some(v) = self.0.get_mut(&tile) {
            v.retain(|x| *x != e);
            if v.is_empty() {
                self.0.remove(&tile);
            }
        }
    }
}

/// Yields a corpse drops when butchered. Mirrors the legacy `death_system`
/// drops for Wolf and Deer (Wolf: 3 Meat + 1 Skin; Deer: 5 Meat + 2 Skin).
pub fn species_yield(species: CorpseSpecies) -> &'static [(Good, u32)] {
    match species {
        CorpseSpecies::Wolf => &[(Good::Meat, 3), (Good::Skin, 1)],
        CorpseSpecies::Deer => &[(Good::Meat, 5), (Good::Skin, 2)],
    }
}

/// Release a carried corpse at the agent's current tile. Called by plan
/// teardown / rescue preemption / butcher-completion paths. Idempotent:
/// safe to call when `carried_corpse` is already `None`.
///
/// The corpse entity itself is not despawned — `corpse_follow_system` stops
/// snapping it to the agent the next tick, so it stays on the ground.
pub fn drop_corpse(ai: &mut PersonAI) {
    ai.carried_corpse = None;
}

/// Ticks of work the Butcher task takes to process a corpse. Roughly matches
/// `TICKS_TAME` and the Workbench craft cadence — short enough that hunters
/// turn around quickly, long enough to feel like a distinct activity.
pub const BUTCHER_TICKS: u8 = 60;

/// Sequential.
///
/// `PickUpCorpse`: agent has routed adjacent to a `Corpse` (target_entity).
/// On arrival (`Working`), attach the corpse to `PersonAI.carried_corpse`,
/// remove it from `CorpseMap` so other hunters don't target it, and end
/// the task so the plan advances to `HaulCorpse`. If the corpse vanished
/// between dispatch and arrival (decay, rescued by another hunter), end
/// the task — the resolver will pick a new corpse next plan iteration.
pub fn pickup_corpse_task_system(
    clock: Res<SimClock>,
    mut corpse_map: ResMut<CorpseMap>,
    corpse_q: Query<(&Corpse, &Transform)>,
    mut agents: Query<(&mut PersonAI, &BucketSlot, &LodLevel)>,
) {
    for (mut ai, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::PickUpCorpse as u16 || ai.state != AiState::Working {
            continue;
        }

        let Some(corpse_e) = ai.target_entity else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };

        if let Ok((_, t)) = corpse_q.get(corpse_e) {
            let tile = (
                (t.translation.x / TILE_SIZE).floor() as i32,
                (t.translation.y / TILE_SIZE).floor() as i32,
            );
            corpse_map.remove(tile, corpse_e);
            ai.carried_corpse = Some(corpse_e);
        }
        // Whether or not the corpse still existed, this dispatch is done —
        // a missing corpse just means the next plan iteration retargets.
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.target_entity = None;
    }
}

/// Sequential.
///
/// `HaulCorpse`: agent walks to the butcher site, with `corpse_follow_system`
/// dragging the corpse alongside. The executor only flips the task to Idle
/// once arrival fires (`Working`), so `plan_execution_system` can advance.
/// If the corpse was lost mid-haul (rescue preempt forgot to clear, decay)
/// the haul still ends — Butcher will fail-fast on a missing corpse and
/// abort the plan cleanly.
pub fn haul_corpse_task_system(
    clock: Res<SimClock>,
    mut agents: Query<(&mut PersonAI, &BucketSlot, &LodLevel)>,
) {
    for (mut ai, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::HaulCorpse as u16 || ai.state != AiState::Working {
            continue;
        }
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}

/// Sequential.
///
/// `Butcher`: agent stands on/near the butcher site with the corpse still
/// attached. `work_progress` advances each tick; on completion we read the
/// corpse's species, drop `species_yield()` as `GroundItem`s at the agent's
/// tile, despawn the corpse, award Crafting XP, emit an Activity log entry,
/// and end the task. The corpse entity is also unlinked from
/// `PersonAI.carried_corpse` so future plans see clean state.
pub fn butcher_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    mut activity: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut item_query: Query<&mut GroundItem>,
    corpse_q: Query<&Corpse>,
    mut agents: Query<(
        Entity,
        &mut PersonAI,
        &Transform,
        &BucketSlot,
        &LodLevel,
        &mut Skills,
        Option<&FactionMember>,
    )>,
) {
    for (entity, mut ai, transform, slot, lod, mut skills, member) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::Butcher as u16 || ai.state != AiState::Working {
            continue;
        }

        let Some(corpse_e) = ai.carried_corpse else {
            // Corpse was dropped (rescue preempt) — abort this dispatch.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };

        let Ok(corpse) = corpse_q.get(corpse_e) else {
            // Corpse despawned (decayed or stolen). Clear and abort.
            ai.carried_corpse = None;
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            continue;
        };

        ai.work_progress = ai.work_progress.saturating_add(1);
        if ai.work_progress < BUTCHER_TICKS {
            continue;
        }

        // Completion: drop species yield at the agent's tile.
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let species = corpse.species;
        for &(good, qty) in species_yield(species) {
            spawn_or_merge_ground_item(&mut commands, &spatial, &mut item_query, tx, ty, good, qty);
        }
        skills.gain_xp(SkillKind::Crafting, 5);

        let activity_name: &'static str = match species {
            CorpseSpecies::Wolf => "Butchered Wolf",
            CorpseSpecies::Deer => "Butchered Deer",
        };
        activity.send(crate::ui::activity_log::ActivityLogEvent {
            tick: clock.tick,
            actor: entity,
            faction_id: member.map(|m| m.faction_id).unwrap_or(0),
            kind: crate::ui::activity_log::ActivityEntryKind::Crafted { name: activity_name },
        });

        commands.entity(corpse_e).despawn_recursive();
        ai.carried_corpse = None;
        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
    }
}

/// Sequential.
///
/// `HuntPartyMuster`: hunter has walked to the chief's chosen muster tile
/// (`StepTarget::HearthForHunt`). On arrival (`Working`), the agent registers
/// itself in the faction's `HuntOrder::Hunt::mustered` list and stays put
/// until the party fills (`mustered.len() >= target_party_size`) or the
/// chief flips `deployed_tick` (the first agent to cross the threshold writes
/// it, gating dispatch for everyone). On stale orders (timeout reached without
/// deployment, or order cleared by the chief invalidation sweep), the executor
/// exits as a soft failure so the plan ends and the next selection cycle
/// re-considers the hunter.
pub fn wait_for_party_task_system(
    clock: Res<SimClock>,
    mut registry: ResMut<FactionRegistry>,
    mut agents: Query<(
        Entity,
        &mut PersonAI,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (entity, mut ai, member, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.task_id != TaskKind::HuntPartyMuster as u16 || ai.state != AiState::Working {
            continue;
        }
        let Some(faction) = registry.factions.get_mut(&member.faction_id) else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };
        let Some(order) = faction.hunt_order.as_mut() else {
            // Chief cleared the order — abort the wait.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        };
        match order {
            HuntOrder::Hunt {
                mustered,
                target_party_size,
                deployed_tick,
                posted_tick,
                ..
            } => {
                if !mustered.contains(&entity) {
                    mustered.push(entity);
                }
                let mustered_len = mustered.len() as u8;
                if deployed_tick.is_none() && mustered_len >= *target_party_size {
                    *deployed_tick = Some(clock.tick);
                }
                let ready = deployed_tick.is_some();
                let stale = clock.tick.saturating_sub(*posted_tick) > HUNT_PARTY_TIMEOUT;
                if ready || stale {
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
                // else: keep waiting (state stays Working).
            }
            HuntOrder::Scout { .. } => {
                // Order flipped from Hunt to Scout while we were mustering.
                // Bail so the plan ends and the next pick can be ScoutForPrey.
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
            }
        }
    }
}

/// Despawns corpses past their freshness window. No carrion drops — a
/// rotted corpse is wasted. Removes the entity from `CorpseMap`.
pub fn corpse_decay_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut corpse_map: ResMut<CorpseMap>,
    mut carriers: Query<&mut PersonAI>,
    q: Query<(Entity, &Corpse, &Transform)>,
) {
    let now = clock.tick;
    for (e, corpse, transform) in &q {
        if now < corpse.fresh_until_tick {
            continue;
        }
        let tile = (
            (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
            (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
        );
        corpse_map.remove(tile, e);
        // Anyone currently dragging this corpse needs their reference cleared.
        for mut ai in &mut carriers {
            if ai.carried_corpse == Some(e) {
                ai.carried_corpse = None;
            }
        }
        commands.entity(e).despawn_recursive();
    }
}

/// While a `PersonAI.carried_corpse` is set, snap the corpse's transform
/// to the carrier's transform each tick so it visibly follows them.
/// Schedule after `movement_system` so the corpse lands on the carrier's
/// new tile before any tile-based read.
pub fn corpse_follow_system(
    carriers: Query<(&PersonAI, &Transform), Without<Corpse>>,
    mut corpses: Query<&mut Transform, With<Corpse>>,
) {
    for (ai, t) in &carriers {
        if let Some(corpse_e) = ai.carried_corpse {
            if let Ok(mut ct) = corpses.get_mut(corpse_e) {
                ct.translation.x = t.translation.x;
                ct.translation.y = t.translation.y;
                // Keep corpse Z slightly below the carrier so it renders as ground litter.
                ct.translation.z = 0.4;
            }
        }
    }
}
