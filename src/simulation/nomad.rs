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
use crate::world::seasons::{TICKS_PER_DAY, TICKS_PER_SEASON};

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
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;

    for (&fid, faction) in registry.factions.iter_mut() {
        if !faction.lifestyle.is_nomadic() {
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
                faction.home_tile = p.target;
                faction.last_migration_tick = now;
                faction.pending_migration = None;
            }
        }
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

fn pick_migration_target(
    shared: &SharedKnowledge,
    wild_herds: &WildHerdRegistry,
    fid: u32,
    home: (i32, i32),
    min_d: i32,
    max_d: i32,
) -> Option<(i32, i32)> {
    // Score = estimated population at the candidate tile. Known food
    // clusters use their direct `estimated_count`; wild herds get a
    // proportional bonus from `aggregate_count` so a 120-head herd
    // outranks all but the very richest known clusters. Nomads thus
    // drift toward visible herds even when the band hasn't recently
    // sighted edible vegetation in that direction.
    let mut best: Option<((i32, i32), i32)> = None;

    if let Some(map) = shared.map(KnowledgeTier::Faction(fid)) {
        for c in map.clusters.values() {
            if !matches!(c.kind, MemoryKind::AnyEdible) {
                continue;
            }
            let d = chebyshev(c.center, home);
            if d < min_d || d > max_d {
                continue;
            }
            let score = c.estimated_count as i32;
            if best.map_or(true, |(_, s)| score > s) {
                best = Some((c.center, score));
            }
        }
    }

    // Wild herds — score weighted as half the aggregate count so a
    // 120-head herd contributes 60 score, comfortably outranking a
    // typical 4-rep-tile food cluster (estimated_count ≤ 4).
    for herd in wild_herds.herds.values() {
        let d = chebyshev(herd.leader_tile, home);
        if d < min_d || d > max_d {
            continue;
        }
        let score = (herd.aggregate_count as i32 / 2).max(20);
        if best.map_or(true, |(_, s)| score > s) {
            best = Some((herd.leader_tile, score));
        }
    }

    best.map(|(t, _)| t)
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
    mut registry: ResMut<FactionRegistry>,
    clock: Res<SimClock>,
    members: Query<(Entity, &crate::simulation::faction::FactionMember)>,
    mut log_events: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;
    // Pre-bucket one actor per faction so we can fire the ActivityLogEvent
    // without a second query pass.
    let mut actor_per_faction: ahash::AHashMap<u32, Entity> = ahash::AHashMap::new();
    for (entity, m) in members.iter() {
        actor_per_faction.entry(m.faction_id).or_insert(entity);
    }
    for (&fid, faction) in registry.factions.iter_mut() {
        if !faction.lifestyle.is_nomadic() {
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
        let camp = faction.home_tile;
        faction.lifestyle = crate::simulation::faction::Lifestyle::Settled;
        if let Some(&actor) = actor_per_faction.get(&fid) {
            log_events.send(crate::ui::activity_log::ActivityLogEvent {
                tick: now as u64,
                actor,
                faction_id: fid,
                kind: crate::ui::activity_log::ActivityEntryKind::Sedentarized {
                    camp,
                },
            });
        }
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
