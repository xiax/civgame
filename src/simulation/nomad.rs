//! Nomadic-mode systems: migration trigger + commit.
//!
//! Two-phase pipeline. `nomad_migration_system` (Economy, daily) decides
//! whether each nomadic band wants to move and writes a target tile into
//! `FactionData.pending_migration`. The trailing `nomad_migration_commit_system`
//! (Sequential, every tick) finds factions with a pending target, tears down
//! the old camp's deployable structures within `OLD_CAMP_RADIUS` of the
//! current `home_tile`, then updates `home_tile = target` and clears the
//! pending flag.
//!
//! MVP commit semantics: despawn-only — no refund drops, no re-seed at the
//! new camp. The chief's `nomad_chief_directives` (Phase 7 follow-on) will
//! own replenishment of lost shelter; for now nomads sleep in-place via
//! `Task::Sleep { bed: None }` at the new home until they rebuild.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::simulation::animals::{AnimalAI, Tamed};
use crate::simulation::construction::{
    best_hearth_for, seed_nomadic_camp, Bed, BedMap, Campfire, CampfireMap, FurnitureMaps,
    TentShelter,
};
use crate::simulation::faction::FactionRegistry;
use crate::simulation::memory::MemoryKind;
use crate::simulation::pack_deploy::Deployable;
use crate::simulation::schedule::SimClock;
use crate::simulation::shared_knowledge::{KnowledgeTier, SharedKnowledge};
use crate::simulation::technology::current_era;
use crate::simulation::wild_herd::WildHerdRegistry;
use crate::world::chunk::ChunkMap;
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::{Biome, Globe};
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY, TICKS_PER_SEASON};
use crate::world::tile::TileKind;
use std::collections::VecDeque;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MigrationStage {
    PackingCamp,
    EnRoute,
    Arrived,
}

#[derive(Clone, Debug)]
pub struct MigrationOrder {
    pub target_tile: (i32, i32),
    pub stage: MigrationStage,
    pub started_tick: u32,
}

/// Tiles within `NOMAD_FORAGE_RADIUS` are "local" — clusters here count
/// toward the band's at-camp food score.
pub const NOMAD_FORAGE_RADIUS: i32 = 24;

/// Min chebyshev distance from old camp the migration target must satisfy.
pub const NOMAD_MIN_TARGET_DIST: i32 = 25;

/// Cap on target distance — keeps migrations from teleporting bands across
/// the world when their knowledge map happens to surface a far-away cluster.
pub const NOMAD_MAX_TARGET_DIST: i32 = 60;

/// P3: composite-score helpers — each helper returns a signed score that's
/// summed into `MigrationScore.total`. Constants tuned so a dominant food
/// cluster (estimated_count ~4) still wins against a weak biome bonus, but
/// equal food candidates choose the better water/season/safety position.
pub const WATER_PROBE_RADIUS: i32 = 8;
pub const RECENT_CAMP_TTL: u32 = TICKS_PER_SEASON * 2;
pub const RECENT_CAMP_RING_CAP: usize = 6;
const PREDATOR_PROBE_RADIUS: i32 = 6;

/// P1: per-agent component pinning the destination of an in-flight
/// migration. Inserted on every band member by `nomad_migration_commit_system`
/// after `home_tile` flips; removed by `nomad_migration_arrival_system`
/// on arrival or timeout.
#[derive(Component, Clone, Copy, Debug)]
pub struct MigrationTarget {
    pub tile: (i32, i32),
    pub started_tick: u32,
    /// Tick of the last successful `assign_task_with_routing` in
    /// `nomad_migration_dispatch_system`. Used by the arrival system's
    /// stall-release path: if dispatch never advances this for an Idle /
    /// UNEMPLOYED agent (Drafted, PlayerOrder, or otherwise filtered by
    /// the dispatcher), they release after `MIGRATE_STALL_TICKS` instead
    /// of waiting out the 3-day hard timeout.
    pub last_dispatched_tick: u32,
}

/// P1: chebyshev arrival radius around the new camp. Below this, the
/// agent's `MigrationTarget` is stripped + their goal cleared so the
/// next 200-tick goal-eval picks a normal need-driven goal.
pub const MIGRATE_ARRIVAL_RADIUS: i32 = 4;

/// P1: hard timeout. After this many ticks of carrying a `MigrationTarget`,
/// the agent gives up — covers stuck-in-impassable-tile edge cases so a
/// lost member doesn't carry the marker forever.
pub const MIGRATE_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 3;

/// Stall-release window. If `last_dispatched_tick` hasn't advanced in
/// this many ticks and the agent is sitting Idle / UNEMPLOYED, the
/// arrival system releases the marker. Catches `Drafted` / `PlayerOrder`
/// members the dispatcher's filter never serves, plus genuinely stranded
/// agents whose path-worker keeps rejecting routes.
pub const MIGRATE_STALL_TICKS: u32 = TICKS_PER_DAY / 2;

/// Despawn radius for the old camp on commit. Sized to cover the seed
/// helpers' outer-ring tents (radius 5..=7 around each hearth, plus a
/// safety margin for offset hearth layouts).
pub const OLD_CAMP_RADIUS: i32 = 12;

/// Trigger pass — Economy, every `TICKS_PER_DAY`. For each nomadic band
/// past its `TICKS_PER_SEASON` cooldown whose local food cluster score is
/// below `members × 3`, picks a target tile in distance band
/// `NOMAD_MIN_TARGET_DIST..=NOMAD_MAX_TARGET_DIST` and stamps
/// `FactionData.pending_migration`. Doesn't touch `home_tile` — that's
/// the commit pass's job.
pub fn nomad_migration_system(
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    shared: Res<SharedKnowledge>,
    wild_herds: Res<WildHerdRegistry>,
    chunk_map: Res<ChunkMap>,
    globe: Res<Globe>,
    calendar: Res<Calendar>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;

    for (&fid, faction) in registry.factions.iter_mut() {
        // Capability check: only mobile-home archetypes migrate.
        if !faction.caps.home.is_mobile() {
            continue;
        }
        if faction.member_count == 0 {
            continue;
        }
        if faction.pending_migration.is_some() {
            continue; // commit pass hasn't drained the previous order yet
        }
        if now < faction.last_migration_tick.saturating_add(TICKS_PER_SEASON) {
            continue;
        }

        let home = faction.home_tile;
        let food_score = score_local_food(&shared, fid, home, NOMAD_FORAGE_RADIUS);
        let threshold: u16 = (faction.member_count.max(1) as u16).saturating_mul(3);
        if food_score >= threshold {
            continue;
        }

        let target = pick_migration_target(
            &shared,
            &wild_herds,
            &chunk_map,
            &globe,
            calendar.season,
            &faction.recent_camps,
            now,
            fid,
            home,
            NOMAD_MIN_TARGET_DIST,
            NOMAD_MAX_TARGET_DIST,
        )
        .unwrap_or_else(|| fallback_direction(fid, home, now));

        info!(
            "Faction {fid} migration triggered ({:?} -> {:?}) tick {now}; food_score={food_score}, threshold={threshold}",
            home, target,
        );
        faction.pending_migration = Some(target);
    }
}

/// Commit pass — Sequential, every tick (exclusive system). Drains every
/// faction's `pending_migration`: despawns Beds/Bedrolls/Campfires/Tents/
/// Yurts within `OLD_CAMP_RADIUS` of the current `home_tile`, removes them
/// from `BedMap` / `CampfireMap`, then re-seeds a fresh camp at the target
/// tile via `seed_nomadic_camp` and stamps `last_migration_tick`.
///
/// Exclusive (`&mut World`) because it touches several SystemParam bundles
/// (`FurnitureMaps`, `Commands`, multiple Queries) that together blow past
/// Bevy's 16-param ceiling. Early-outs cheaply when no faction has a
/// pending order.
pub fn nomad_migration_commit_system(world: &mut World) {
    // Snapshot pending migrations + the per-faction context the seeder
    // needs (member count, era for tier selection). Done first so the
    // registry borrow drops before we hand the world to other system
    // states.
    struct Pending {
        fid: u32,
        old_home: (i32, i32),
        target: (i32, i32),
        members: u32,
        era: crate::simulation::technology::Era,
        hearth_tier: crate::simulation::construction::HearthTier,
    }

    let pending: Vec<Pending> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                f.pending_migration.map(|target| Pending {
                    fid,
                    old_home: f.home_tile,
                    target,
                    members: f.member_count,
                    era: current_era(&f.techs),
                    hearth_tier: best_hearth_for(&f.techs),
                })
            })
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let now = world.resource::<SimClock>().tick as u32;

    // ── P5: pre-migration band redistribution ───────────────────────
    // Even out essentials (bedroll, packed_yurt, preserved_meat) across
    // band members before the despawn / pack pass runs. Avoids the case
    // where one member at 99% capacity strands the band's only yurt.
    // Snapshot-then-writeback keeps disjoint mutable borrows simple.
    {
        let migrating: ahash::AHashSet<u32> = pending.iter().map(|p| p.fid).collect();
        let essentials = crate::simulation::nomad_pool::essentials_for_band();
        let mut state: SystemState<(
            Res<FactionRegistry>,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &mut crate::economy::agent::EconomicAgent,
            )>,
        )> = SystemState::new(world);
        let (registry, mut q) = state.get_mut(world);
        let mut updates: ahash::AHashMap<Entity, crate::economy::agent::EconomicAgent> =
            ahash::AHashMap::new();
        for &fid in migrating.iter() {
            let mut snapshot: Vec<(Entity, crate::economy::agent::EconomicAgent)> = q
                .iter()
                .filter(|(_, m, _)| registry.root_faction(m.faction_id) == fid)
                .map(|(e, _, a)| (e, *a))
                .collect();
            if snapshot.len() < 2 {
                continue;
            }
            let mut view: Vec<(Entity, &mut crate::economy::agent::EconomicAgent)> =
                snapshot.iter_mut().map(|(e, a)| (*e, &mut *a)).collect();
            let report = crate::simulation::nomad_pool::redistribute_essentials(
                &mut view,
                &essentials,
            );
            if report.units_moved == 0 {
                continue;
            }
            for (e, a) in snapshot.into_iter() {
                updates.insert(e, a);
            }
        }
        for (e, _, mut agent) in q.iter_mut() {
            if let Some(updated) = updates.get(&e) {
                *agent = *updated;
            }
        }
        state.apply(world);
    }

    // ── P8: pack pass ─────────────────────────────────────────────
    // Walk fully-packable Deployables (Bedrolls/Yurts) within
    // OLD_CAMP_RADIUS of each migrating band. Convert each to its
    // `packed_form` good and place onto the nearest tamed pack animal
    // with capacity, falling back to the nearest band member's
    // EconomicAgent. Tents (refund-only) are skipped here — the despawn
    // pass below drops their refund.
    {
        let migrating: ahash::AHashMap<u32, (i32, i32)> =
            pending.iter().map(|p| (p.fid, p.old_home)).collect();
        let mut state: SystemState<(
            Query<(Entity, &Transform, &Deployable)>,
            Query<
                (Entity, &Transform, &Tamed, &mut crate::simulation::animals::PackAnimalInventory),
            >,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &Transform,
                &mut crate::economy::agent::EconomicAgent,
            )>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (deployable_q, mut animal_q, mut member_q, registry) = state.get_mut(world);

        // Snapshot packable shelters per migrating faction (we don't
        // know faction ownership from the entity directly — match by
        // proximity to the OLD home tile instead). For each shelter,
        // route packed_form to the nearest pack animal or member.
        for (e, transform, deploy) in deployable_q.iter() {
            let Some(packed_rid) = deploy.packed_form else {
                continue; // refund-only forms (Tents) skip the pack pass
            };
            let tile = transform_tile(transform);
            // Find the migrating faction whose old-home is closest to
            // this shelter (within OLD_CAMP_RADIUS). Skips shelters
            // outside any migrating band's camp footprint.
            let mut owner: Option<u32> = None;
            let mut best_dist = i32::MAX;
            for (&fid, &old_home) in migrating.iter() {
                let d = chebyshev(tile, old_home);
                if d <= OLD_CAMP_RADIUS && d < best_dist {
                    best_dist = d;
                    owner = Some(fid);
                }
            }
            let Some(fid) = owner else {
                continue;
            };
            // Try the closest tamed pack animal with capacity for one unit.
            let unit_w = packed_rid.unit_weight_g().max(1);
            let mut chosen_animal: Option<(Entity, i32)> = None;
            for (a_e, a_t, tamed, inv) in animal_q.iter() {
                if registry.root_faction(tamed.owner_faction) != fid {
                    continue;
                }
                if inv.free_capacity_g() < unit_w {
                    continue;
                }
                let a_tile = transform_tile(a_t);
                let d = chebyshev(a_tile, tile);
                if chosen_animal.map_or(true, |(_, prev_d)| d < prev_d) {
                    chosen_animal = Some((a_e, d));
                }
            }
            if let Some((a_e, _)) = chosen_animal {
                if let Ok((_, _, _, mut inv)) = animal_q.get_mut(a_e) {
                    let unfit = inv.add(packed_rid, 1);
                    if unfit == 0 {
                        // Successfully packed — let the despawn pass
                        // remove the entity. We don't despawn here since
                        // the despawn pass also handles BedMap cleanup.
                        let _ = e;
                        continue;
                    }
                }
            }
            // Fall back: nearest member's EconomicAgent.
            let mut chosen_member: Option<(Entity, i32)> = None;
            for (m_e, member, m_t, agent) in member_q.iter() {
                if registry.root_faction(member.faction_id) != fid {
                    continue;
                }
                if agent.free_capacity_g() < unit_w {
                    continue;
                }
                let m_tile = transform_tile(m_t);
                let d = chebyshev(m_tile, tile);
                if chosen_member.map_or(true, |(_, prev_d)| d < prev_d) {
                    chosen_member = Some((m_e, d));
                }
            }
            if let Some((m_e, _)) = chosen_member {
                if let Ok((_, _, _, mut agent)) = member_q.get_mut(m_e) {
                    let _unfit = agent.add_resource(packed_rid, 1);
                    // Even if fallback can't accept (heavy yurt vs full
                    // member), we still let the despawn pass remove the
                    // shelter — better to lose 1 packed shelter than to
                    // leave a structure orphaned at the abandoned camp.
                }
            }
        }
        state.apply(world);
    }

    // ── Despawn pass + refund drops ─────────────────────────────────
    // Walk BedMap / CampfireMap, then the Deployable / TentShelter
    // queries, and despawn anything within `OLD_CAMP_RADIUS` of any
    // band's old home. For Deployable entities with a non-zero refund
    // (sticks-and-leaves Tents), drop `refund_pct * refund_qty` of
    // `refund_resource` as a `GroundItem` at the entity's tile before
    // despawning — fulfills the "wooden sticks and leaves you get maybe
    // 50% back" design contract for Tents.
    {
        let mut despawn_state: SystemState<(
            Commands,
            ResMut<BedMap>,
            ResMut<CampfireMap>,
            Res<crate::world::spatial::SpatialIndex>,
            Query<&mut crate::simulation::items::GroundItem>,
            Query<&Deployable>,
            Query<(Entity, &Transform), With<Deployable>>,
            Query<(Entity, &Transform), With<TentShelter>>,
        )> = SystemState::new(world);
        let (
            mut commands,
            mut bed_map,
            mut campfire_map,
            spatial,
            mut ground_q,
            deployable_data_q,
            deployable_q,
            tent_q,
        ) = despawn_state.get_mut(world);

        for p in pending.iter() {
            // Track entities we've already despawned this pass so the
            // belt-and-braces sweep below doesn't double-drop refunds.
            let mut despawned: ahash::AHashSet<Entity> = ahash::AHashSet::new();

            // Bed entries (covers Bed + Bedroll).
            let bed_tiles: Vec<(i32, i32)> = bed_map
                .0
                .keys()
                .copied()
                .filter(|t| chebyshev(*t, p.old_home) <= OLD_CAMP_RADIUS)
                .collect();
            for tile in bed_tiles {
                if let Some(entity) = bed_map.0.remove(&tile) {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
                }
            }

            // Campfire entries.
            let fire_tiles: Vec<(i32, i32)> = campfire_map
                .0
                .keys()
                .copied()
                .filter(|t| chebyshev(*t, p.old_home) <= OLD_CAMP_RADIUS)
                .collect();
            for tile in fire_tiles {
                if let Some(entity) = campfire_map.0.remove(&tile) {
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
                }
            }

            // TentShelter entities (no map).
            for (entity, transform) in tent_q.iter() {
                if despawned.contains(&entity) {
                    continue;
                }
                let tile = transform_tile(transform);
                if chebyshev(tile, p.old_home) <= OLD_CAMP_RADIUS {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                    despawned.insert(entity);
                }
            }

            // Belt-and-braces: stray Deployables not picked up above.
            for (entity, transform) in deployable_q.iter() {
                if despawned.contains(&entity) {
                    continue;
                }
                let tile = transform_tile(transform);
                if chebyshev(tile, p.old_home) <= OLD_CAMP_RADIUS {
                    drop_refund_at_tile(
                        &deployable_data_q,
                        entity,
                        tile,
                        &mut commands,
                        &spatial,
                        &mut ground_q,
                    );
                    commands.entity(entity).despawn_recursive();
                }
            }
        }
        despawn_state.apply(world);
    }

    // ── Re-seed pass ────────────────────────────────────────────────
    // Reuse `seed_nomadic_camp` so the new camp matches the game-start
    // layout (hearth ring + bedrolls + tents + Neo+ yurts). Run one
    // SystemState per migration since `seed_nomadic_camp` mutates the
    // command buffer + furniture maps each call.
    for p in pending.iter() {
        let mut seed_state: SystemState<(
            Commands,
            FurnitureMaps,
            Res<ChunkMap>,
            EventWriter<TileChangedEvent>,
        )> = SystemState::new(world);
        let (mut commands, mut maps, chunk_map, mut tile_changed) = seed_state.get_mut(world);
        let mut used: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
        used.insert(p.target);
        seed_nomadic_camp(
            &mut commands,
            &mut maps,
            &chunk_map,
            &mut tile_changed,
            &mut used,
            p.fid,
            p.target,
            p.members,
            p.era,
            p.hearth_tier,
        );
        seed_state.apply(world);
    }

    // ── Registry mutation ───────────────────────────────────────────
    // Capture each faction's chief (or any first member) for the
    // ActivityLogEvent's `actor`, then mutate registry state.
    let mut actor_per_faction: ahash::AHashMap<u32, Entity> = ahash::AHashMap::new();
    {
        let mut state: SystemState<
            Query<(Entity, &crate::simulation::faction::FactionMember)>,
        > = SystemState::new(world);
        let q = state.get(world);
        for (entity, member) in q.iter() {
            actor_per_faction.entry(member.faction_id).or_insert(entity);
        }
    }
    {
        let mut registry = world.resource_mut::<FactionRegistry>();
        for p in pending.iter() {
            if let Some(faction) = registry.factions.get_mut(&p.fid) {
                // P3: push the now-vacated camp tile into the recent-camps
                // ring before mutating home_tile, so the next migration
                // pick penalises returning here.
                faction.recent_camps.push_back((p.old_home, now));
                while faction.recent_camps.len() > RECENT_CAMP_RING_CAP {
                    faction.recent_camps.pop_front();
                }
                faction.home_tile = p.target;
                faction.last_migration_tick = now;
                faction.pending_migration = None;
            }
        }
    }

    // ── P1: stamp every band member with `MigrationTarget` + flip their
    // goal to MigrateToCamp so the dispatcher actively walks them with
    // the band. Survive-tier needs (raid / starvation / rescue) preempt
    // naturally in `goal_update_system`.
    {
        let migrating: ahash::AHashMap<u32, (i32, i32)> =
            pending.iter().map(|p| (p.fid, p.target)).collect();
        let mut state: SystemState<(
            Commands,
            Query<(
                Entity,
                &crate::simulation::faction::FactionMember,
                &mut crate::simulation::goals::AgentGoal,
                &mut crate::simulation::person::PersonAI,
                &mut crate::simulation::typed_task::ActionQueue,
            )>,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (mut commands, mut q, registry) = state.get_mut(world);
        for (e, member, mut goal, mut ai, mut aq) in q.iter_mut() {
            let root = registry.root_faction(member.faction_id);
            let Some(&target) = migrating.get(&root) else {
                continue;
            };
            commands.entity(e).insert(MigrationTarget {
                tile: target,
                started_tick: now,
                last_dispatched_tick: now,
            });
            *goal = crate::simulation::goals::AgentGoal::MigrateToCamp;
            // Cancel current chain so the dispatcher picks up MigrateToCamp
            // immediately instead of finishing a pre-migration gather.
            aq.cancel();
            ai.task_id = crate::simulation::person::PersonAI::UNEMPLOYED;
            ai.state = crate::simulation::person::AiState::Idle;
        }
        state.apply(world);
    }

    // ── Activity log ────────────────────────────────────────────────
    // Emit one CampMoved per migrated faction so the player's UI shows
    // "moved camp (x,y) → (x',y')". Chief or first-found member is the
    // notional actor.
    {
        let mut state: SystemState<
            EventWriter<crate::ui::activity_log::ActivityLogEvent>,
        > = SystemState::new(world);
        let mut writer = state.get_mut(world);
        for p in pending.iter() {
            let Some(&actor) = actor_per_faction.get(&p.fid) else {
                continue;
            };
            writer.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now as u64,
                actor,
                faction_id: p.fid,
                kind: crate::ui::activity_log::ActivityEntryKind::CampMoved {
                    from: p.old_home,
                    to: p.target,
                },
            });
        }
        state.apply(world);
    }

    // ── Phase 5 minimum: Tamed animals follow camp ──────────────────
    // Redirect every Tamed animal whose `owner_faction` just migrated to
    // wander toward the new camp tile. The animal_movement_system then
    // walks them there at standard ANIMAL_SPEED. Members of nomadic
    // bands' herds (tamed horses, etc.) thus drift with the camp instead
    // of being abandoned at the old site.
    {
        let mut tamed_state: SystemState<Query<(&Tamed, &mut AnimalAI)>> =
            SystemState::new(world);
        let mut tamed_q = tamed_state.get_mut(world);
        // Build a small map from faction_id → new camp tile so we can
        // O(1) look up the redirect target per animal.
        let new_homes: ahash::AHashMap<u32, (i32, i32)> =
            pending.iter().map(|p| (p.fid, p.target)).collect();
        for (tamed, mut ai) in tamed_q.iter_mut() {
            let Some(target) = new_homes.get(&tamed.owner_faction) else {
                continue;
            };
            // Offset the redirect by ±2 tiles so the herd spreads out
            // around the camp rather than stacking on a single tile.
            let seed = tamed.owner_faction.wrapping_mul(0x85EB_CA6B);
            let dx = ((seed & 0b11) as i32) - 2;
            let dy = (((seed >> 2) & 0b11) as i32) - 2;
            ai.target_tile = (target.0 + dx, target.1 + dy);
        }
    }

    for p in pending.iter() {
        info!(
            "Faction {} migration committed ({:?} -> {:?}) tick {now}",
            p.fid, p.old_home, p.target,
        );
    }
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Helper for the migration commit despawn pass. Looks up the entity's
/// `Deployable` data; if it carries a non-zero refund (sticks-and-leaves
/// Tent), spawns a `GroundItem` at the entity's tile via
/// `spawn_or_merge_ground_item`. No-op for Bedrolls / Yurts (their
/// `packed_form` covers the materials).
fn drop_refund_at_tile(
    deployable_q: &Query<&Deployable>,
    entity: Entity,
    tile: (i32, i32),
    commands: &mut Commands,
    spatial: &crate::world::spatial::SpatialIndex,
    ground_q: &mut Query<&mut crate::simulation::items::GroundItem>,
) {
    let Ok(deployable) = deployable_q.get(entity) else {
        return;
    };
    let Some((rid, qty)) = deployable.compute_refund_drop() else {
        return;
    };
    crate::simulation::items::spawn_or_merge_ground_item(
        commands, spatial, ground_q, tile.0, tile.1, rid, qty,
    );
}

#[inline]
fn transform_tile(transform: &Transform) -> (i32, i32) {
    use crate::world::terrain::TILE_SIZE;
    let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
    let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
    (tx, ty)
}

fn score_local_food(
    shared: &SharedKnowledge,
    fid: u32,
    home: (i32, i32),
    radius: i32,
) -> u16 {
    let Some(map) = shared.map(KnowledgeTier::Faction(fid)) else {
        return 0;
    };
    let mut score: u16 = 0;
    for c in map.clusters.values() {
        if !matches!(c.kind, MemoryKind::AnyEdible) {
            continue;
        }
        if chebyshev(c.center, home) <= radius {
            score = score.saturating_add(c.estimated_count);
        }
    }
    score
}

/// Composite score for a candidate migration target tile. `total` is the
/// authoritative ranking field; sub-scores are exposed for debug/inspect.
#[derive(Clone, Copy, Debug, Default)]
pub struct MigrationScore {
    pub food: i32,
    pub herd: i32,
    pub water: i32,
    pub biome_season: i32,
    pub danger: i32,
    pub recency: i32,
    pub total: i32,
}

/// P3 picker. Composite-scores known food clusters + wild-herd leaders,
/// adding water/biome-season bonuses and predator/recency penalties; picks
/// the highest-total candidate within the distance band.
#[allow(clippy::too_many_arguments)]
pub fn pick_migration_target(
    shared: &SharedKnowledge,
    wild_herds: &WildHerdRegistry,
    chunk_map: &ChunkMap,
    globe: &Globe,
    season: Season,
    recent_camps: &VecDeque<((i32, i32), u32)>,
    now: u32,
    fid: u32,
    home: (i32, i32),
    min_d: i32,
    max_d: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<((i32, i32), MigrationScore)> = None;

    let mut consider = |tile: (i32, i32), food: i32, herd: i32| {
        let d = chebyshev(tile, home);
        if d < min_d || d > max_d {
            return;
        }
        let water = score_water(chunk_map, tile, WATER_PROBE_RADIUS);
        let biome_season = score_biome_season(globe, tile, season);
        let danger = score_danger(shared, fid, tile);
        let recency = score_recency(recent_camps, tile, now);
        let total = food + herd + water + biome_season + danger + recency;
        let score = MigrationScore {
            food,
            herd,
            water,
            biome_season,
            danger,
            recency,
            total,
        };
        if best.map_or(true, |(_, s)| total > s.total) {
            best = Some((tile, score));
        }
    };

    if let Some(map) = shared.map(KnowledgeTier::Faction(fid)) {
        for c in map.clusters.values() {
            if !matches!(c.kind, MemoryKind::AnyEdible) {
                continue;
            }
            consider(c.center, c.estimated_count as i32, 0);
        }
    }
    for herd in wild_herds.herds.values() {
        // Wild herd score mirrors the legacy weighting: a 120-head herd
        // contributes 60, comfortably outranking a typical 4-rep cluster.
        let herd_score = (herd.aggregate_count as i32 / 2).max(20);
        consider(herd.leader_tile, 0, herd_score);
    }

    best.map(|(t, _)| t)
}

/// +30 at the candidate tile when adjacent water; falls off ~3 per chebyshev
/// tile, capped at 0 beyond `WATER_PROBE_RADIUS`. Fresh water (rivers) adds a
/// flat `+10` so a band picks a riverside camp over an equidistant salt
/// coast. Bands strongly prefer camps with reliable water access.
pub fn score_water(chunk_map: &ChunkMap, tile: (i32, i32), radius: i32) -> i32 {
    for r in 0..=radius {
        for dx in -r..=r {
            for dy in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // outline only — concentric expansion
                }
                if let Some(kind) = chunk_map.tile_kind_at(tile.0 + dx, tile.1 + dy) {
                    if kind.is_water_like() {
                        let base = (30 - r * 3).max(0);
                        let fresh = if kind.is_freshwater() { 10 } else { 0 };
                        return base + fresh;
                    }
                }
            }
        }
    }
    0
}

/// Per-biome × per-season suitability bonus. Winter penalises Tundra /
/// Mountain; Summer penalises Desert; Grassland gets a year-round bonus
/// (rich forage); Ocean is a hard reject.
pub fn score_biome_season(globe: &Globe, tile: (i32, i32), season: Season) -> i32 {
    let biome = crate::world::biome::classify_at_tile(globe, tile.0, tile.1);
    let base: i32 = match biome {
        Biome::Ocean => -100,
        Biome::Grassland => 15,
        Biome::Steppe => 12,
        Biome::Temperate => 10,
        Biome::Wetland => 8,
        Biome::Tropical => 5,
        Biome::Taiga => 0,
        Biome::Desert => -5,
        Biome::Tundra => -5,
        Biome::Badlands => -8,
        Biome::Mountain => -10,
    };
    let seasonal: i32 = match (biome, season) {
        (Biome::Tundra | Biome::Mountain, Season::Winter) => -15,
        (Biome::Desert | Biome::Badlands, Season::Summer) => -15,
        (Biome::Wetland, Season::Summer) => -8, // mosquito / disease load
        (Biome::Grassland | Biome::Steppe, Season::Spring | Season::Summer) => 5,
        (Biome::Tropical, Season::Winter) => 5,
        _ => 0,
    };
    base + seasonal
}

/// Penalises tiles near sighted predator/prey clusters (proxy for "wolves
/// hunt this area"). −15 per `MemoryKind::Prey` cluster centre within
/// `PREDATOR_PROBE_RADIUS`. A wolf-pack-rich tile thus pulls 15..45 below
/// a quiet alternative — enough to flip equal-food candidates.
pub fn score_danger(shared: &SharedKnowledge, fid: u32, tile: (i32, i32)) -> i32 {
    let Some(map) = shared.map(KnowledgeTier::Faction(fid)) else {
        return 0;
    };
    let mut penalty: i32 = 0;
    for c in map.clusters.values() {
        if !matches!(c.kind, MemoryKind::Prey) {
            continue;
        }
        if chebyshev(c.center, tile) <= PREDATOR_PROBE_RADIUS {
            penalty -= 15;
        }
    }
    penalty
}

/// Penalises tiles near recent camp sites. Decays with age over
/// `RECENT_CAMP_TTL`. A freshly-vacated tile within 8 chebyshev gets
/// −25; older entries fade to ~0 as their age approaches the TTL.
pub fn score_recency(
    recent_camps: &VecDeque<((i32, i32), u32)>,
    tile: (i32, i32),
    now: u32,
) -> i32 {
    let mut penalty: i32 = 0;
    for &(pos, when) in recent_camps.iter() {
        if chebyshev(pos, tile) >= 8 {
            continue;
        }
        let age = now.saturating_sub(when);
        if age >= RECENT_CAMP_TTL {
            continue;
        }
        // Linear decay from -25 at age=0 to 0 at age=TTL.
        let factor = 1.0 - (age as f32 / RECENT_CAMP_TTL as f32);
        penalty -= (25.0 * factor) as i32;
    }
    penalty
}

/// Stable-camp duration before a nomadic faction may sedentarize. One
/// full game-year = 4 seasons. Bands moving more often than annually
/// stay nomadic indefinitely.
pub const NOMAD_SEDENTARIZE_TICKS: u32 = TICKS_PER_SEASON * 4;

/// Min member count for sedentarization. Small bands keep moving — they
/// need enough hands to build huts + walls before food runs out.
pub const NOMAD_SEDENTARIZE_MIN_MEMBERS: u32 = 12;

/// Phase 11: nomadic → settled lifestyle conversion. Economy, daily.
///
/// A nomadic faction that has stayed in one camp for ≥ `NOMAD_SEDENTARIZE_TICKS`
/// (one full game-year) AND has ≥ `NOMAD_SEDENTARIZE_MIN_MEMBERS` adults
/// flips `lifestyle = Settled`. From the next tick:
/// - `auto_found_default_settlements_system` founds a `Settlement` at the
///   current camp tile (which becomes the permanent home).
/// - `carve_plots_system` carves plots in the resulting `SettlementPlan`.
/// - `chief_directive_system` and `chief_job_posting_system` re-engage,
///   queuing huts/walls/granaries.
/// - `compute_faction_storage_system` switches back to the
///   `FactionStorageTile` rollup — but the storage tile doesn't exist yet,
///   so faction.storage.totals briefly reads 0 until the chief posts a
///   build for one (settled bands seed their storage tile at spawn; a
///   newly-sedentarized band would need it queued separately, follow-on).
///
/// Reverse direction (settled → nomadic on collapse) is deferred.
pub fn nomad_sedentarize_system(
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    mut lifecycle_queue: ResMut<crate::simulation::lifecycle::LifecycleEventQueue>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;
    for (&fid, faction) in registry.factions.iter() {
        // Capability check: only mobile-home archetypes can sedentarize.
        if !faction.caps.home.is_mobile() {
            continue;
        }
        if faction.member_count < NOMAD_SEDENTARIZE_MIN_MEMBERS {
            continue;
        }
        if faction.pending_migration.is_some() {
            continue; // about to move; not stable
        }
        // last_migration_tick == 0 means "never moved since spawn" — we
        // treat the spawn tick (0) as the start of the stay, so a faction
        // that hasn't migrated for a full year sedentarizes naturally.
        let stay_duration = now.saturating_sub(faction.last_migration_tick);
        if stay_duration < NOMAD_SEDENTARIZE_TICKS {
            continue;
        }
        info!(
            "Faction {fid} sedentarized (stable for {stay_duration} ticks at {:?}) tick {now}",
            faction.home_tile,
        );
        // P3: emit SwitchArchetype event. The lifecycle processor
        // (exclusive World, runs later in this tick) executes the
        // 7-step re-derivation: caps + land_policy + economic_policy
        // re-applied, old camp structures despawned, culture_hash
        // bumped, FactionStorageTile spawned synchronously, and the
        // `Sedentarized` activity log event emitted.
        let new_key = crate::simulation::lifecycle::settled_variant_of(
            &faction.caps.archetype_key,
        );
        lifecycle_queue.push(
            crate::simulation::lifecycle::SettlementLifecycleEvent::SwitchArchetype {
                faction: fid,
                new_archetype_key: new_key,
                at_tile: faction.home_tile,
            },
        );
    }
}

fn fallback_direction(fid: u32, home: (i32, i32), now: u32) -> (i32, i32) {
    let seed = fid.wrapping_mul(0x9E37_79B9).wrapping_add(now);
    let dir = (seed % 8) as i32;
    let (dx, dy) = match dir {
        0 => (35, 0),
        1 => (25, 25),
        2 => (0, 35),
        3 => (-25, 25),
        4 => (-35, 0),
        5 => (-25, -25),
        6 => (0, -35),
        _ => (25, -25),
    };
    (home.0 + dx, home.1 + dy)
}

/// P2 (slim nomad chief): per-faction shelter targets used by
/// `nomad_chief_directive_system` to size replacement blueprint queues.
fn nomad_shelter_targets(members: u32) -> NomadShelterTargets {
    NomadShelterTargets {
        bedrolls: members,
        tents: ((members + 3) / 4).max(1),
        yurts: (members / 5).clamp(1, 2),
    }
}

#[derive(Copy, Clone, Debug)]
pub struct NomadShelterTargets {
    pub bedrolls: u32,
    pub tents: u32,
    pub yurts: u32,
}

/// P2: max bps the nomad chief queues per tick (bounded so a brand-new
/// camp doesn't get carpet-bombed with 30 blueprints all at once).
const NOMAD_DIRECTIVE_BP_PER_TICK: usize = 2;

/// P2: scan radius around `home_tile` for shelter counts + new-blueprint
/// placement. Aligns with the seed/nomad_camp footprint.
const NOMAD_DIRECTIVE_RADIUS: i32 = 8;

/// P2: slim chief for nomadic bands. Daily, queues replacement Bedroll /
/// Tent / Yurt blueprints when the camp's shelter falls below the
/// per-member targets. Posts no jobs (members do autonomous gathering);
/// the existing `gather` / `scavenge` HTN methods + the new
/// `nomad_band_pool_balance_system` (P5) handle materials end-to-end.
#[allow(clippy::too_many_arguments)]
pub fn nomad_chief_directive_system(
    mut commands: Commands,
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    mut bp_map: ResMut<crate::simulation::construction::BlueprintMap>,
    bed_map: Res<crate::simulation::construction::BedMap>,
    tent_q: Query<(&Transform, &crate::simulation::construction::TentShelter)>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
) {
    use crate::simulation::construction::{
        next_clear_tile, BuildSiteKind, Blueprint, ShelterTier,
    };

    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now_tick = clock.tick;
    for (&fid, faction) in registry.factions.iter() {
        if !faction.caps.home.is_mobile() {
            continue;
        }
        if faction.pending_migration.is_some() {
            continue;
        }
        if faction.member_count == 0 {
            continue;
        }
        let home = faction.home_tile;
        let targets = nomad_shelter_targets(faction.member_count);

        // Count built shelter within radius of home.
        let bedroll_built = bed_map
            .0
            .keys()
            .filter(|&&t| chebyshev(t, home) <= NOMAD_DIRECTIVE_RADIUS)
            .count() as u32;
        let mut tent_built: u32 = 0;
        let mut yurt_built: u32 = 0;
        for (t_t, shelter) in tent_q.iter() {
            let tile = transform_tile(t_t);
            if chebyshev(tile, home) > NOMAD_DIRECTIVE_RADIUS {
                continue;
            }
            match shelter.tier {
                ShelterTier::Tent => tent_built += 1,
                ShelterTier::Yurt => yurt_built += 1,
            }
        }

        // Count pending blueprints (avoid re-queueing).
        let mut bedroll_pending: u32 = 0;
        let mut tent_pending: u32 = 0;
        let mut yurt_pending: u32 = 0;
        for bp in bp_query.iter() {
            if bp.faction_id != fid {
                continue;
            }
            if chebyshev(bp.tile, home) > NOMAD_DIRECTIVE_RADIUS {
                continue;
            }
            match bp.kind {
                BuildSiteKind::Bedroll => bedroll_pending += 1,
                BuildSiteKind::Tent => tent_pending += 1,
                BuildSiteKind::Yurt => yurt_pending += 1,
                _ => {}
            }
        }

        let mut budget = NOMAD_DIRECTIVE_BP_PER_TICK;
        let mut used: ahash::AHashSet<(i32, i32)> = bp_map.0.keys().copied().collect();
        // Helper: queue one Single blueprint of `kind` near home.
        let queue_one = |budget: &mut usize,
                         used: &mut ahash::AHashSet<(i32, i32)>,
                         bp_map: &mut crate::simulation::construction::BlueprintMap,
                         commands: &mut Commands,
                         kind: BuildSiteKind|
         -> bool {
            if *budget == 0 {
                return false;
            }
            let tile = match next_clear_tile(home, used, &chunk_map, NOMAD_DIRECTIVE_RADIUS) {
                Some(t) => t,
                None => return false,
            };
            let target_z = chunk_map.surface_z_at(tile.0, tile.1) as i8;
            use crate::world::terrain::tile_to_world;
            let wp = tile_to_world(tile.0, tile.1);
            let e = commands
                .spawn((
                    Blueprint::new(fid, None, kind, tile, target_z),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
            used.insert(tile);
            *budget -= 1;
            true
        };

        // Priority order: bedrolls (every member sleeps), then tents
        // (group shelter), then yurts (advanced, Neolithic+ tech-gated
        // by recipe).
        if bedroll_built + bedroll_pending < targets.bedrolls {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Bedroll,
            );
        }
        if tent_built + tent_pending < targets.tents {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Tent,
            );
        }
        if yurt_built + yurt_pending < targets.yurts
            && faction.techs.has(crate::simulation::technology::PORTABLE_DWELLINGS)
        {
            queue_one(
                &mut budget,
                &mut used,
                &mut bp_map,
                &mut commands,
                BuildSiteKind::Yurt,
            );
        }
        let _ = now_tick;
    }
}

/// P1 dispatcher — ParallelB. For every agent carrying a `MigrationTarget`
/// whose goal is `MigrateToCamp` and who is otherwise idle, dispatch
/// `Task::WalkTo { tile, why: Migration }` via `assign_task_with_routing`.
/// Bucket-gated like other ParallelB dispatchers via `BucketSlot`.
///
/// Self-heals two failure modes the plain "queue is Idle" gate would
/// otherwise leave permanently parked:
/// - A stale `Task::WalkTo { why: Migration }` left on `aq.current` by
///   `movement::release_to_idle` (which clears `task_id` but not `aq`).
///   `goal_dispatch_system`'s stale-reset is gated on
///   `task_id != UNEMPLOYED` and so misses this case.
/// - A target whose chunk is in a different connectivity component than
///   the agent's: routing always "succeeds" at this layer for non-adjacent
///   tasks, then the path worker rejects it forever. We fail fast here
///   and release the marker so the agent re-evaluates next tick.
#[allow(clippy::too_many_arguments)]
pub fn nomad_migration_dispatch_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_graph: Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_map: Res<ChunkMap>,
    chunk_connectivity: Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    mut q: Query<
        (
            Entity,
            &mut MigrationTarget,
            &mut crate::simulation::goals::AgentGoal,
            &Transform,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &crate::simulation::schedule::BucketSlot,
            &crate::simulation::lod::LodLevel,
        ),
        Without<crate::simulation::person::Drafted>,
    >,
) {
    use crate::simulation::lod::LodLevel;
    use crate::simulation::person::{AiState, PersonAI};
    use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
    use crate::simulation::typed_task::{Task, WalkReason};
    use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
    use crate::world::terrain::TILE_SIZE;

    let now = clock.tick as u32;
    for (e, mut target, mut goal, transform, mut ai, mut aq, slot, lod) in q.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if !clock.is_active(slot.0) {
            continue;
        }
        if *goal != crate::simulation::goals::AgentGoal::MigrateToCamp {
            continue;
        }
        if ai.task_id != PersonAI::UNEMPLOYED {
            continue;
        }
        // Self-heal: `release_to_idle` (movement.rs) wipes `task_id` and
        // `state` after a path-worker failure but does not touch
        // `aq.current` — leaving a stale `Task::WalkTo { Migration }` that
        // would otherwise block re-dispatch forever. Drop it here so the
        // route below can run.
        if !matches!(aq.current, Task::Idle) {
            let stale_migration_walk = matches!(
                aq.current,
                Task::WalkTo { why: WalkReason::Migration, .. },
            );
            if stale_migration_walk && ai.state == AiState::Idle {
                aq.cancel();
            } else {
                continue;
            }
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        // Already arrived? Skip — arrival system will strip the marker.
        if chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS {
            continue;
        }
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        // Connectivity fast-fail. `assign_task_with_routing` only
        // connectivity-checks for adjacent-task targets; for `Migrate` it
        // always returns true and the path worker would just keep
        // rejecting the request. Release the marker so the agent picks a
        // normal goal next tick instead of cycling dispatch ↔ path-fail.
        let target_chunk = ChunkCoord(
            target.tile.0.div_euclid(CHUNK_SIZE as i32),
            target.tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        let target_z = chunk_map.nearest_standable_z(
            target.tile.0,
            target.tile.1,
            ai.current_z as i32,
        ) as i8;
        if !chunk_connectivity
            .is_reachable((cur_chunk, ai.current_z), (target_chunk, target_z))
        {
            commands.entity(e).remove::<MigrationTarget>();
            *goal = crate::simulation::goals::AgentGoal::GatherFood;
            aq.cancel();
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.state = AiState::Idle;
            continue;
        }
        let routed = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            target.tile,
            TaskKind::Migrate,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !routed {
            continue;
        }
        ai.state = AiState::Routing;
        let z = ai.target_z;
        aq.dispatch(Task::WalkTo {
            tile: target.tile,
            z,
            why: WalkReason::Migration,
        });
        target.last_dispatched_tick = now;
    }
}

/// P1 arrival check — Sequential, after movement_system. Sweeps every
/// agent with a `MigrationTarget`; on chebyshev arrival within
/// `MIGRATE_ARRIVAL_RADIUS`, after `MIGRATE_TIMEOUT_TICKS`, or after
/// `MIGRATE_STALL_TICKS` of dispatch inactivity (Drafted / PlayerOrder /
/// stranded agents), removes the marker, drops back to Idle, and clears
/// the goal so the next 200-tick goal-eval picks a normal need-driven
/// goal.
pub fn nomad_migration_arrival_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut q: Query<(
        Entity,
        &MigrationTarget,
        &Transform,
        &mut crate::simulation::goals::AgentGoal,
        &mut crate::simulation::person::PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
    )>,
) {
    use crate::simulation::person::{AiState, PersonAI};
    use crate::world::terrain::TILE_SIZE;
    let now = clock.tick as u32;
    for (e, target, transform, mut goal, mut ai, mut aq) in q.iter_mut() {
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let arrived = chebyshev((cur_tx, cur_ty), target.tile) <= MIGRATE_ARRIVAL_RADIUS;
        let timed_out = now.saturating_sub(target.started_tick) > MIGRATE_TIMEOUT_TICKS;
        // Stall release: dispatch hasn't advanced `last_dispatched_tick`
        // for a while and the agent is sitting Idle / UNEMPLOYED — either
        // they're filtered out of the dispatcher (Drafted, PlayerOrder)
        // or stranded by repeated path-worker failures. Either way, no
        // further forward progress will happen on its own.
        let stalled = ai.task_id == PersonAI::UNEMPLOYED
            && ai.state == AiState::Idle
            && now.saturating_sub(target.last_dispatched_tick) > MIGRATE_STALL_TICKS;
        if !(arrived || timed_out || stalled) {
            continue;
        }
        commands.entity(e).remove::<MigrationTarget>();
        if *goal == crate::simulation::goals::AgentGoal::MigrateToCamp {
            *goal = crate::simulation::goals::AgentGoal::GatherFood;
        }
        // Stop the walk; a normal goal will pick up next tick.
        aq.cancel();
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.state = AiState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_recency_penalises_freshly_vacated_camp() {
        let mut camps: VecDeque<((i32, i32), u32)> = VecDeque::new();
        camps.push_back(((10, 10), 0));
        // Tile near the recent camp, age=0 → strong negative.
        let near = score_recency(&camps, (12, 11), 0);
        assert!(near < -20, "fresh near-camp penalty should be ~ -25; got {near}");
        // Far tile gets nothing.
        let far = score_recency(&camps, (50, 50), 0);
        assert_eq!(far, 0);
        // Aged-out entry gets nothing.
        let aged = score_recency(&camps, (10, 10), RECENT_CAMP_TTL + 1);
        assert_eq!(aged, 0);
    }

    #[test]
    fn score_biome_season_winter_penalises_tundra() {
        // Use a default Globe — `classify_at_tile` will return whatever
        // biome the noise picks, but Tundra/Mountain/Ocean are all
        // negative regardless of season; the seasonal modifier just
        // doubles down. We can't deterministically place a tile in
        // tundra without seeding, so this test exercises the matrix
        // logic by walking biomes directly.
        for biome in [
            Biome::Tundra,
            Biome::Mountain,
            Biome::Desert,
            Biome::Grassland,
        ] {
            let summer = score_biome_season_for_biome(biome, Season::Summer);
            let winter = score_biome_season_for_biome(biome, Season::Winter);
            match biome {
                Biome::Tundra | Biome::Mountain => {
                    assert!(winter < summer, "{:?} winter should be worse than summer; w={winter} s={summer}", biome);
                }
                Biome::Desert => {
                    assert!(summer < winter, "Desert summer should be worse than winter; w={winter} s={summer}");
                }
                Biome::Grassland => {
                    assert!(summer >= winter, "Grassland summer should ≥ winter; w={winter} s={summer}");
                }
                _ => {}
            }
        }
    }

    /// Biome-season scoring extracted for unit-testing without a Globe.
    /// Mirrors the per-(biome, season) table in `score_biome_season`.
    fn score_biome_season_for_biome(biome: Biome, season: Season) -> i32 {
        let base: i32 = match biome {
            Biome::Ocean => -100,
            Biome::Grassland => 15,
            Biome::Steppe => 12,
            Biome::Temperate => 10,
            Biome::Wetland => 8,
            Biome::Tropical => 5,
            Biome::Taiga => 0,
            Biome::Desert => -5,
            Biome::Tundra => -5,
            Biome::Badlands => -8,
            Biome::Mountain => -10,
        };
        let seasonal: i32 = match (biome, season) {
            (Biome::Tundra | Biome::Mountain, Season::Winter) => -15,
            (Biome::Desert | Biome::Badlands, Season::Summer) => -15,
            (Biome::Wetland, Season::Summer) => -8,
            (Biome::Grassland | Biome::Steppe, Season::Spring | Season::Summer) => 5,
            (Biome::Tropical, Season::Winter) => 5,
            _ => 0,
        };
        base + seasonal
    }

    /// Stall release: an agent the dispatcher never serves (here simulated
    /// via `Drafted`, which excludes the agent from the dispatcher and
    /// from `goal_update_system`'s normal selection — the same code path
    /// hunters / lecture attendees take during migration) must still get
    /// out of `Goal::MigrateToCamp` once `MIGRATE_STALL_TICKS` elapses.
    /// Without this we'd be paying the 3-day `MIGRATE_TIMEOUT_TICKS`
    /// fallback for every drafted member of a migrating band.
    #[test]
    fn arrival_stall_releases_drafted_agent() {
        use crate::simulation::test_fixture::TestSim;
        use crate::world::tile::TileKind;
        use bevy::prelude::*;

        let mut sim = TestSim::new(0xBA11D);
        sim.flat_world(1, 0, TileKind::Grass);
        let agent = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});

        // Tick once so all the Startup / first-frame systems settle and
        // the agent acquires its components.
        sim.tick();

        let started_tick = sim.tick_count() as u32;
        // Stamp MigrationTarget at a far tile and lock the agent into
        // MigrateToCamp. `Drafted` keeps both the dispatcher and
        // `goal_update_system`'s normal selection from touching them.
        sim.app.world_mut().entity_mut(agent).insert((
            MigrationTarget {
                tile: (50, 50),
                started_tick,
                last_dispatched_tick: started_tick,
            },
            crate::simulation::goals::AgentGoal::MigrateToCamp,
            crate::simulation::person::Drafted,
        ));

        // Walk past the stall threshold. arrival_system runs every
        // tick, so the stall path should fire once the gap exceeds
        // MIGRATE_STALL_TICKS.
        sim.tick_n(MIGRATE_STALL_TICKS + 5);

        assert!(
            sim.app.world().get::<MigrationTarget>(agent).is_none(),
            "MigrationTarget should be removed by stall arrival path",
        );
        let goal = sim
            .app
            .world()
            .get::<crate::simulation::goals::AgentGoal>(agent)
            .copied();
        assert_eq!(
            goal,
            Some(crate::simulation::goals::AgentGoal::GatherFood),
            "stall arrival should flip MigrateToCamp → GatherFood",
        );
    }

    /// Within `MIGRATE_ARRIVAL_RADIUS` of the target, the regular arrival
    /// path still releases the marker — this is the "happy path" that
    /// the existing migration pipeline already exercises end-to-end, but
    /// pin it explicitly so a regression in the stall-path edits doesn't
    /// break it.
    #[test]
    fn arrival_radius_releases_when_at_target_tile() {
        use crate::simulation::test_fixture::TestSim;
        use crate::world::tile::TileKind;

        let mut sim = TestSim::new(0xBA12D);
        sim.flat_world(1, 0, TileKind::Grass);
        // Spawn directly on the target tile — chebyshev = 0, well inside
        // `MIGRATE_ARRIVAL_RADIUS`.
        let agent = sim.spawn_person(sim.player_faction_id, (0, 0), |_| {});
        sim.tick();
        let now = sim.tick_count() as u32;
        sim.app.world_mut().entity_mut(agent).insert((
            MigrationTarget {
                tile: (0, 0),
                started_tick: now,
                last_dispatched_tick: now,
            },
            crate::simulation::goals::AgentGoal::MigrateToCamp,
        ));
        // One tick is enough — arrival runs in Sequential after movement.
        sim.tick_n(2);
        assert!(sim.app.world().get::<MigrationTarget>(agent).is_none());
    }
}
