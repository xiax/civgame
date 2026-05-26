use ahash::AHashMap;
use bevy::prelude::*;

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

/// Yields a corpse drops when butchered. Wolf: 3 Meat + 1 Skin + 1 Bone;
/// Deer: 5 Meat + 2 Skin + 2 Bone. Bone is the crudest tool stock (Realistic
/// Tool Overhaul) — Bone Awl / Bone Fishing Kit.
pub fn species_yield(
    species: CorpseSpecies,
) -> [(crate::economy::resource_catalog::ResourceId, u32); 3] {
    use crate::economy::core_ids;
    let meat = core_ids::meat();
    let skin = core_ids::skin();
    let bone = core_ids::bone();
    match species {
        CorpseSpecies::Wolf => [(meat, 3), (skin, 1), (bone, 1)],
        CorpseSpecies::Deer => [(meat, 5), (skin, 2), (bone, 2)],
    }
}

/// Marker component: this entity is currently dragging a `Corpse`. Inserted
/// by `pickup_corpse_task_system` on arrival, removed by butcher completion,
/// rescue preempt, decay system, and various teardown paths. The
/// `corpse_follow_system` keys on this component to snap the corpse's
/// transform to the carrier each tick.
///
/// Replaces `PersonAI.carried_corpse: Option<Entity>` (Phase 3d follow-up
/// refactor): the carrying state spans 3 tasks (PickUpCorpse → HaulCorpse →
/// Butcher) so it lives at the component level rather than as a task-local
/// param. Insert/Remove is the natural ECS shape; the previous Option-field
/// required every teardown path to remember to clear it.
#[derive(bevy::prelude::Component, Copy, Clone, Debug)]
pub struct Carrying(pub bevy::prelude::Entity);

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
    mut commands: Commands,
    clock: Res<SimClock>,
    mut corpse_map: ResMut<CorpseMap>,
    corpse_q: Query<(&Corpse, &Transform)>,
    mut agents: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (entity, mut ai, mut aq, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::PickUpCorpse as u16 || ai.state != AiState::Working {
            continue;
        }

        // Phase 3d-iii: corpse comes from typed `Task::PickUpCorpse`, falling
        // back to `target_entity` for any unmigrated dispatch path.
        let Some(corpse_e) = aq.current.as_pickup_corpse().or(ai.target_entity) else {
            aq.finish_task(&mut ai);
            continue;
        };

        if let Ok((_, t)) = corpse_q.get(corpse_e) {
            let tile = (
                (t.translation.x / TILE_SIZE).floor() as i32,
                (t.translation.y / TILE_SIZE).floor() as i32,
            );
            corpse_map.remove(tile, corpse_e);
            commands.entity(entity).insert(Carrying(corpse_e));
        }
        // Whether or not the corpse still existed, this dispatch is done —
        // a missing corpse just means the next plan iteration retargets.
        ai.target_entity = None;
        aq.finish_task(&mut ai);
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
    mut agents: Query<(
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut aq, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::HaulCorpse as u16 || ai.state != AiState::Working {
            continue;
        }
        aq.finish_task(&mut ai);
        // Phase 5e-viii-a chain handoff: when the queued head was
        // `Task::Butcher` (DeliverHuntKillMethod's tail), `aq.advance()`
        // just promoted it into `aq.current`. Prime the legacy channel so
        // `butcher_task_system` picks up next tick (Butcher is in-place —
        // no routing). Mirrors `production::finish_withdraw_food`'s Eat
        // handoff. Plan-driven dispatch (legacy PlanId 5 was truncated, but
        // any in-flight plan with stale step state would land here too)
        // will re-prime via `plan_execution_system` next tick anyway, so
        // this in-place priming is HTN-specific and benign for legacy.
        if aq.current.is_butcher() {
            aq.begin_working(&mut ai);
        }
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
        &mut crate::simulation::typed_task::ActionQueue,
        &Transform,
        &BucketSlot,
        &LodLevel,
        &mut Skills,
        Option<&FactionMember>,
        Option<&Carrying>,
        Option<&crate::simulation::apprenticeship::ApprenticeOf>,
    )>,
) {
    for (entity, mut ai, mut aq, transform, slot, lod, mut skills, member, carrying, apprentice) in
        agents.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Butcher as u16 || ai.state != AiState::Working {
            continue;
        }

        let Some(Carrying(corpse_e)) = carrying.copied() else {
            // Carrying was removed (rescue preempt) — abort this dispatch.
            aq.finish_task(&mut ai);
            continue;
        };

        let Ok(corpse) = corpse_q.get(corpse_e) else {
            // Corpse despawned (decayed or stolen). Clear and abort.
            commands.entity(entity).remove::<Carrying>();
            aq.finish_task(&mut ai);
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
        for &(rid, qty) in species_yield(species).iter() {
            spawn_or_merge_ground_item(&mut commands, &spatial, &mut item_query, tx, ty, rid, qty);
        }
        // Phase 5b: deliberate-practice multiplier for apprentices —
        // butchering counts as a craft-skill activity.
        let xp = crate::simulation::apprenticeship::xp_with_apprentice_bonus(5, apprentice);
        skills.gain_xp(SkillKind::Crafting, xp);

        let activity_name: &'static str = match species {
            CorpseSpecies::Wolf => "Butchered Wolf",
            CorpseSpecies::Deer => "Butchered Deer",
        };
        activity.send(crate::ui::activity_log::ActivityLogEvent {
            tick: clock.tick,
            actor: entity,
            faction_id: member.map(|m| m.faction_id).unwrap_or(0),
            kind: crate::ui::activity_log::ActivityEntryKind::Crafted {
                name: activity_name,
            },
        });

        commands.entity(corpse_e).despawn_recursive();
        commands.entity(entity).remove::<Carrying>();
        aq.finish_task(&mut ai);
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
        &mut crate::simulation::typed_task::ActionQueue,
        &FactionMember,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (entity, mut ai, mut aq, member, slot, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::HuntPartyMuster as u16
            || ai.state != AiState::Working
        {
            continue;
        }
        let Some(faction) = registry.factions.get_mut(&member.faction_id) else {
            aq.finish_task(&mut ai);
            continue;
        };
        let Some(order) = faction.hunt_order.as_mut() else {
            // Chief cleared the order — abort the wait.
            aq.finish_task(&mut ai);
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
                    aq.finish_task(&mut ai);
                }
                // else: keep waiting (state stays Working).
            }
            HuntOrder::Scout { .. } => {
                // Order flipped from Hunt to Scout while we were mustering.
                // Bail so the plan ends and the next pick can be ScoutForPrey.
                aq.finish_task(&mut ai);
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
    carriers: Query<(Entity, &Carrying)>,
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
        // Anyone currently dragging this corpse needs their Carrying removed.
        for (carrier_e, carrying) in &carriers {
            if carrying.0 == e {
                commands.entity(carrier_e).remove::<Carrying>();
            }
        }
        commands.entity(e).despawn_recursive();
        // Rotting body → waste pile at the corpse's tile. Falls outside
        // any Latrine, so contamination spreads at full intensity until
        // `sanitation_decay_system` ages it out.
        let world_pos = transform.translation;
        commands.spawn((
            Transform::from_xyz(world_pos.x, world_pos.y, 0.1),
            GlobalTransform::default(),
            crate::simulation::sanitation::WastePile {
                intensity: 1.5,
                created_tick: now,
            },
        ));
    }
}

/// While a carrier has a `Carrying(corpse_e)` component, snap the corpse's
/// transform to the carrier's transform each tick so it visibly follows them.
/// Schedule after `movement_system` so the corpse lands on the carrier's
/// new tile before any tile-based read.
pub fn corpse_follow_system(
    carriers: Query<(&Carrying, &Transform), Without<Corpse>>,
    mut corpses: Query<&mut Transform, With<Corpse>>,
) {
    for (carrying, t) in &carriers {
        let corpse_e = carrying.0;
        {
            if let Ok(mut ct) = corpses.get_mut(corpse_e) {
                ct.translation.x = t.translation.x;
                ct.translation.y = t.translation.y;
                // Keep corpse Z slightly below the carrier so it renders as ground litter.
                ct.translation.z = 0.4;
            }
        }
    }
}
