//! Observable HTN slow-path for nomadic pack, unload, and pitch labor.
//!
//! When the player issues `PlayerCommand::PackCamp`, the band's
//! `CampState` flips to `Packed` immediately (the goal gate stops
//! settled-life work), but the shelters don't despawn synchronously
//! — instead `dispatch_unpitch_tasks` enumerates every Deployable
//! structure inside `OLD_CAMP_RADIUS` of `home_tile`, picks the
//! nearest eligible band member for each, and stamps them with
//! `Task::UnpitchStructure { structure }`. Workers walk to the
//! structure, accumulate `work_progress` for `UNPITCH_WORK_TICKS`,
//! and `unpitch_structure_task_system` then despawns the entity and
//! drops its packed form (or refund) as `GroundItem`s at the tile.
//! Members pick up the goods naturally via existing scavenge / pack
//! animal AI as they migrate. AI autopilot migrations reuse the same
//! pack labor during `MigrationPhase::PackingCamp`; the only special
//! case is that campfires are also dismantled as no-cargo teardown.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::construction::{
    Bed, BedMap, BuildSiteKind, Campfire, CampfireMap, ShelterTier, StructureLabel, TentShelter,
};
use crate::simulation::faction::{
    release_reservation, FactionMember, FactionRegistry, StorageReservations,
};
use crate::simulation::gather::spawn_ground_drop;
use crate::simulation::gather_claims::{release_gather_claim, GatherClaims};
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::GroundItem;
use crate::simulation::jobs::{release_claimant, ClaimTarget, JobBoard, JobClaim};
use crate::simulation::lod::LodLevel;
use crate::simulation::pack_deploy::Deployable;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::spatial::{Indexed, IndexedKind};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

/// Work ticks each `UnpitchStructure` task accumulates before the
/// structure despawns. ~2 seconds at 20 Hz feels like real labor
/// without dragging the pack-up out unbearably for a 12-shelter camp.
pub const UNPITCH_WORK_TICKS: u32 = 40;

/// Work ticks for final-destination `PitchStructureAt` tasks. Kept equal
/// to unpitching for now so a full caravan move has visible labor on
/// both sides without making the destination setup crawl.
pub const PITCH_WORK_TICKS: u32 = 40;

/// Cadence for `continue_pack_labor_system`. Fires often enough that
/// idle workers pick up the next shelter before they wander, but not
/// every tick (cheap polling).
pub const PACK_LABOR_REDISPATCH_INTERVAL: u64 = 10;

/// Marker inserted on every band member when a player issues
/// `PackCamp`. Commits them to the pack pipeline:
///
/// - `goal_update_system` skips autonomous goal re-evaluation while
///   this marker is present, so hunger / sleep / mobile-gate don't
///   flip them off pack labor.
/// - `continue_pack_labor_system` keeps assigning fresh
///   `UnpitchStructure` tasks to idle members until every Deployable
///   inside the camp's pack radius has been dismantled — at which
///   point the marker is stripped and members resume normal AI
///   (eat / sleep / scavenge / respond to player Move orders).
/// - Player `Move` / other commands still override (the `Commanded`
///   check in `goal_update_system` runs first, and player-command
///   dispatch calls `aq.cancel()` to drop any in-flight Unpitch
///   chain).
#[derive(bevy::prelude::Component, Default)]
pub struct PackingDuty;

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

#[inline]
fn transform_tile(transform: &Transform) -> (i32, i32) {
    (
        (transform.translation.x / TILE_SIZE).floor() as i32,
        (transform.translation.y / TILE_SIZE).floor() as i32,
    )
}

/// Snapshot of a camp structure that needs dismantling.
struct StructureToUnpitch {
    fid: u32,
    entity: Entity,
    tile: (i32, i32),
}

/// Insert a `PackingDuty` marker on every non-drafted member of every
/// faction in `fids` and clear any stale autonomous work. Called by
/// `apply_pack_camp_command_system` at the start of a player Pack
/// episode.
pub fn stamp_pack_duty(world: &mut World, fids: &[u32]) {
    if fids.is_empty() {
        return;
    }
    let mut state: SystemState<(
        Commands,
        Query<
            (
                Entity,
                &FactionMember,
                &Transform,
                &mut PersonAI,
                &mut ActionQueue,
                &mut AgentGoal,
                Option<&JobClaim>,
            ),
            (With<Person>, Without<Drafted>),
        >,
        Res<FactionRegistry>,
        Res<StorageReservations>,
        Res<GatherClaims>,
        ResMut<JobBoard>,
    )> = SystemState::new(world);
    let (mut commands, mut q, registry, reservations, gather_claims, mut board) =
        state.get_mut(world);
    for (entity, member, transform, mut ai, mut aq, mut goal, claim) in q.iter_mut() {
        let root = registry.root_faction(member.faction_id);
        if fids.contains(&root) {
            let tile = transform_tile(transform);
            release_reservation(&reservations, &mut ai);
            release_gather_claim(&gather_claims, &mut ai, entity);
            if let Some(claim) = claim {
                release_claimant(&mut board, claim.job_id, entity);
            }

            aq.cancel_chain(&mut ai);
            ai.target_entity = None;
            ai.target_tile = tile;
            ai.dest_tile = tile;
            ai.target_z = ai.current_z;
            ai.work_progress = 0;
            *goal = AgentGoal::FollowingPlayerCommand;

            commands
                .entity(entity)
                .insert(PackingDuty)
                .remove::<JobClaim>()
                .remove::<ClaimTarget>();
        }
    }
    state.apply(world);
}

/// Strip `PackingDuty` from every member of the given factions.
/// Called when `continue_pack_labor_system` confirms the camp has no
/// more Deployables in its pack radius (pack pipeline complete) or
/// when `apply_pitch_camp_command_system` finalises the new camp.
pub fn clear_pack_duty(world: &mut World, fids: &[u32]) {
    if fids.is_empty() {
        return;
    }
    let mut state: SystemState<(
        Commands,
        Query<(Entity, &FactionMember), With<PackingDuty>>,
        Res<FactionRegistry>,
        ResMut<crate::simulation::goals::ForceGoalReevaluate>,
    )> = SystemState::new(world);
    let (mut commands, q, registry, mut force_reeval) = state.get_mut(world);
    for (entity, member) in q.iter() {
        let root = registry.root_faction(member.faction_id);
        if fids.contains(&root) {
            commands.entity(entity).remove::<PackingDuty>();
            force_reeval.0.insert(entity);
        }
    }
    state.apply(world);
}

/// Periodic re-dispatcher: hands idle `PackingDuty` members the next
/// Deployable structure in the camp radius. Runs every
/// `PACK_LABOR_REDISPATCH_INTERVAL` ticks. When no Deployables remain
/// for a faction, the marker is stripped so members resume normal AI.
pub fn continue_pack_labor_system(world: &mut World) {
    let tick = world.resource::<SimClock>().tick;
    if tick % PACK_LABOR_REDISPATCH_INTERVAL != 0 {
        return;
    }

    // Collect every faction that's currently packing. Player flow is
    // keyed on CampState::Packed; AI caravan flow is keyed on the
    // explicit MigrationPhase::PackingCamp, even though CampState is
    // also Packed for the duration.
    let packs: Vec<(u32, (i32, i32), i32)> = {
        let registry = world.resource::<FactionRegistry>();
        registry
            .factions
            .iter()
            .filter_map(|(&fid, f)| {
                if let crate::simulation::faction::MigrationPhase::PackingCamp {
                    old_home,
                    radius,
                    ..
                } = f.migration_phase
                {
                    return Some((fid, old_home, radius));
                }
                if !matches!(
                    f.camp_state,
                    crate::simulation::faction::CampState::Packed { .. }
                ) {
                    return None;
                }
                // AI autopilot factions are handled by PackingCamp
                // above. A Packed AI faction outside that phase is
                // either mid-pitch or recovering from a cancelled route.
                if f.nomad_autopilot {
                    return None;
                }
                let adoption = crate::simulation::technology_adoption::community_adoption_bitset(f);
                let era = crate::simulation::technology::current_era(&adoption);
                let radius =
                    crate::simulation::construction::seed_nomadic_camp_extent(f.member_count, era);
                Some((fid, f.home_tile, radius))
            })
            .collect()
    };
    if packs.is_empty() {
        return;
    }

    // Which factions still have pack targets in their pack radius?
    let remaining_fids = pack_targets_remaining(world, &packs);
    let factions_done: Vec<u32> = packs
        .iter()
        .filter_map(|(fid, _, _)| (!remaining_fids.contains(fid)).then_some(*fid))
        .collect();
    if !factions_done.is_empty() {
        clear_pack_duty(world, &factions_done);
    }

    // For factions with work remaining, re-dispatch UnpitchStructure
    // to any PackingDuty member who is currently UNEMPLOYED. The
    // dispatcher already filters by chebyshev distance to pick the
    // nearest unused worker per structure.
    let remaining: Vec<(u32, (i32, i32), i32)> = packs
        .into_iter()
        .filter(|(fid, _, _)| !factions_done.contains(fid))
        .collect();
    if remaining.is_empty() {
        return;
    }
    dispatch_unpitch_tasks(world, &remaining);
}

/// Return the set of faction ids that still have a pack target in their
/// pack radius. Targets include all `Deployable` shelters plus campfires,
/// which are no-cargo teardown for AI caravan moves.
pub fn pack_targets_remaining(
    world: &mut World,
    packs: &[(u32, (i32, i32), i32)],
) -> ahash::AHashSet<u32> {
    if packs.is_empty() {
        return ahash::AHashSet::default();
    }
    let mut state: SystemState<Query<&Transform, Or<(With<Deployable>, With<Campfire>)>>> =
        SystemState::new(world);
    let q = state.get(world);
    let mut remaining: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for transform in q.iter() {
        let tile = transform_tile(transform);
        for &(fid, home, radius) in packs.iter() {
            if chebyshev(tile, home) <= radius {
                remaining.insert(fid);
                break;
            }
        }
    }
    remaining
}

/// Dispatch a `Task::UnpitchStructure` for every pack target found
/// within `radius` of each pack anchor. Workers are picked by
/// chebyshev distance to the structure; each worker may only be
/// assigned one Unpitch task per pack episode (further structures go
/// to other workers, or queue if no workers remain). Chief is
/// eligible. Drafted / SOLO / non-matching-faction members are not.
pub fn dispatch_unpitch_tasks(world: &mut World, packs: &[(u32, (i32, i32), i32)]) {
    if packs.is_empty() {
        return;
    }

    // ── Snapshot structures ────────────────────────────────────────
    let structures: Vec<StructureToUnpitch> = {
        let mut state: SystemState<
            Query<(Entity, &Transform), Or<(With<Deployable>, With<Campfire>)>>,
        > = SystemState::new(world);
        let q = state.get(world);
        let mut out = Vec::new();
        for (entity, transform) in q.iter() {
            let tile = transform_tile(transform);
            for &(fid, anchor, radius) in packs {
                if chebyshev(tile, anchor) <= radius {
                    out.push(StructureToUnpitch { fid, entity, tile });
                    break;
                }
            }
        }
        out
    };
    if structures.is_empty() {
        return;
    }

    // ── Snapshot eligible workers per faction ──────────────────────
    struct Worker {
        entity: Entity,
        tile: (i32, i32),
        chunk: ChunkCoord,
        z: i8,
    }
    let workers_by_faction: ahash::AHashMap<u32, Vec<Worker>> = {
        let mut state: SystemState<(
            Query<
                (
                    Entity,
                    &FactionMember,
                    &Transform,
                    &PersonAI,
                    &crate::simulation::typed_task::ActionQueue,
                ),
                (With<Person>, With<PackingDuty>, Without<Drafted>),
            >,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        let csz = CHUNK_SIZE as i32;
        let mut acc: ahash::AHashMap<u32, Vec<Worker>> = ahash::AHashMap::default();
        for (entity, member, transform, ai, aq) in q.iter() {
            let root = registry.root_faction(member.faction_id);
            if !packs.iter().any(|(fid, _, _)| *fid == root) {
                continue;
            }
            // Only re-dispatch to workers who are currently UNEMPLOYED.
            // Members already on an UnpitchStructure task stay on it.
            if aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                continue;
            }
            let tile = transform_tile(transform);
            let chunk = ChunkCoord(tile.0.div_euclid(csz), tile.1.div_euclid(csz));
            acc.entry(root).or_default().push(Worker {
                entity,
                tile,
                chunk,
                z: ai.current_z,
            });
        }
        acc
    };

    // ── Assign each structure to the nearest unused worker ─────────
    struct Assignment {
        worker: Entity,
        structure: Entity,
        worker_tile: (i32, i32),
        worker_chunk: ChunkCoord,
        worker_z: i8,
        structure_tile: (i32, i32),
    }
    let mut assignments: Vec<Assignment> = Vec::new();
    let mut used_workers: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    for s in structures.iter() {
        let Some(pool) = workers_by_faction.get(&s.fid) else {
            continue;
        };
        // Pick the nearest unused worker.
        let Some(w) = pool
            .iter()
            .filter(|w| !used_workers.contains(&w.entity))
            .min_by_key(|w| chebyshev(w.tile, s.tile))
        else {
            continue;
        };
        used_workers.insert(w.entity);
        assignments.push(Assignment {
            worker: w.entity,
            structure: s.entity,
            worker_tile: w.tile,
            worker_chunk: w.chunk,
            worker_z: w.z,
            structure_tile: s.tile,
        });
    }
    if assignments.is_empty() {
        return;
    }

    // ── Route + stamp the task ─────────────────────────────────────
    let mut state: SystemState<(
        Res<ChunkMap>,
        Res<ChunkGraph>,
        Res<ChunkRouter>,
        Res<ChunkConnectivity>,
        Res<crate::world::spatial::SpatialIndex>,
        Res<crate::simulation::stand_reservation::StandTileReservations>,
        Res<crate::simulation::SimClock>,
        Query<(&mut PersonAI, &mut ActionQueue, &mut AgentGoal)>,
    )> = SystemState::new(world);
    let (
        chunk_map,
        chunk_graph,
        chunk_router,
        chunk_connectivity,
        spatial_index,
        stand_reservations,
        clock,
        mut q,
    ) = state.get_mut(world);
    let now = clock.tick;
    for a in assignments.iter() {
        let Ok((mut ai, mut aq, mut goal)) = q.get_mut(a.worker) else {
            continue;
        };
        // Stamp `current_z` so routing helper picks the right plane.
        ai.current_z = a.worker_z;
        let routed = assign_task_with_routing(
            &mut ai,
            a.worker_tile,
            a.worker_chunk,
            a.structure_tile,
            TaskKind::UnpitchStructure,
            None,
            Some(a.structure),
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
            &spatial_index,
            &stand_reservations,
            a.worker,
            now,);
        if routed {
            aq.cancel();
            aq.dispatch(Task::UnpitchStructure {
                structure: a.structure,
            });
            *goal = AgentGoal::FollowingPlayerCommand;
        }
    }
    state.apply(world);
}

/// Executor for `Task::UnpitchStructure`. Worker accumulates
/// `work_progress` while adjacent to the structure tile; on
/// completion the structure entity despawns and its packed form
/// goes into the worker's inventory (preferred), or onto the
/// ground as a `GroundItem` if it won't fit. Refund-only (Tent)
/// structures drop the refund as `GroundItem`s — those represent
/// loose materials, not a packed shelter.
pub fn unpitch_structure_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut bed_map: ResMut<BedMap>,
    mut campfire_map: ResMut<CampfireMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    structure_q: Query<(&Transform, Option<&Deployable>, Has<Bed>, Has<Campfire>)>,
    mut workers: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut crate::economy::agent::EconomicAgent,
            &BucketSlot,
            &LodLevel,
        ),
        With<Person>,
    >,
) {
    for (mut ai, mut aq, mut agent, slot, lod) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working
            || aq.current_task_kind() != TaskKind::UnpitchStructure as u16
        {
            continue;
        }
        let Some(structure) = aq.current.as_unpitch_structure() else {
            aq.finish_task(&mut ai);
            continue;
        };
        // Structure gone (raced or already despawned): clean exit.
        let Ok((transform, deploy, is_bed, is_campfire)) = structure_q.get(structure) else {
            aq.finish_task(&mut ai);
            continue;
        };
        if (ai.work_progress as u32) < UNPITCH_WORK_TICKS {
            continue;
        }

        // ── Completion: stash the packed form(s) in the worker's
        // inventory if they fit; spill to ground otherwise so the
        // band can scavenge as it moves.
        let tile = transform_tile(transform);
        let stash_or_drop = |commands: &mut Commands,
                             agent: &mut crate::economy::agent::EconomicAgent,
                             rid: crate::economy::resource_catalog::ResourceId,
                             qty: u32| {
            let mut remaining = qty;
            if remaining > 0 {
                let unfit = agent.add_resource(rid, remaining);
                remaining = unfit;
            }
            if remaining > 0 {
                spawn_ground_drop(commands, tile.0, tile.1, rid, remaining);
            }
        };
        if let Some(deploy) = deploy {
            if let Some(packed_rid) = deploy.packed_form {
                stash_or_drop(&mut commands, &mut agent, packed_rid, 1);
            }
            for (rid, qty) in deploy.packed_bundles.iter() {
                stash_or_drop(&mut commands, &mut agent, *rid, *qty);
            }
            if deploy.packed_form.is_none() && deploy.packed_bundles.is_empty() {
                if let Some((rid, qty)) = deploy.compute_refund_drop() {
                    // Tent-style loose materials: drop on the ground rather
                    // than stash. A wood pile isn't a packed shelter.
                    spawn_ground_drop(&mut commands, tile.0, tile.1, rid, qty);
                }
            }
        }

        // ── Drop from maps if registered there.
        if is_bed {
            bed_map.0.remove(&tile);
        }
        if is_campfire {
            campfire_map.0.remove(&tile);
        }

        commands.entity(structure).despawn_recursive();
        tile_changed.send(TileChangedEvent {
            tx: tile.0,
            ty: tile.1,
        });

        aq.finish_task(&mut ai);
    }
}

/// Executor for `Task::UnloadCampCargo`. Members drop a small stack of
/// carried camp cargo near the destination so the pitch workers can
/// consume it from the ground.
pub fn unload_camp_cargo_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut workers: Query<
        (
            &mut PersonAI,
            &mut ActionQueue,
            &mut crate::economy::agent::EconomicAgent,
            &BucketSlot,
            &LodLevel,
        ),
        With<Person>,
    >,
) {
    for (mut ai, mut aq, mut agent, slot, lod) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working
            || aq.current_task_kind() != TaskKind::UnloadCampCargo as u16
        {
            continue;
        }
        let Some((rid, qty, tile)) = aq.current.as_unload_camp_cargo() else {
            aq.finish_task(&mut ai);
            continue;
        };
        let removed = agent.remove_resource(rid, qty as u32);
        if removed > 0 {
            spawn_ground_drop(&mut commands, tile.0, tile.1, rid, removed);
        }
        aq.finish_task(&mut ai);
    }
}

/// Executor for `Task::PitchStructureAt`. This is the slow final-camp
/// setup path used by AI caravans: bedrolls and yurts consume their
/// unloaded packed goods, while the basic campfire is a no-cargo labor
/// placement at the final anchor.
pub fn pitch_structure_at_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut bed_map: ResMut<BedMap>,
    mut campfire_map: ResMut<CampfireMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    mut ground_q: Query<(Entity, &Transform, &mut GroundItem)>,
    mut workers: Query<(&mut PersonAI, &mut ActionQueue, &BucketSlot, &LodLevel), With<Person>>,
) {
    for (mut ai, mut aq, slot, lod) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working
            || aq.current_task_kind() != TaskKind::PitchStructureAt as u16
        {
            continue;
        }
        let Some((kind, anchor)) = aq.current.as_pitch_structure_at() else {
            aq.finish_task(&mut ai);
            continue;
        };
        if (ai.work_progress as u32) < PITCH_WORK_TICKS {
            continue;
        }

        let already_present = match kind {
            BuildSiteKind::Bedroll | BuildSiteKind::Bed => bed_map.0.contains_key(&anchor),
            BuildSiteKind::Campfire => campfire_map.0.contains_key(&anchor),
            _ => false,
        };
        if already_present {
            finish_pitch_worker(&mut ai, &mut aq);
            continue;
        }

        let needs_good = match kind {
            BuildSiteKind::Bedroll => Some(crate::economy::core_ids::bedroll()),
            BuildSiteKind::Yurt => Some(crate::economy::core_ids::packed_yurt()),
            BuildSiteKind::Campfire => None,
            _ => None,
        };
        if let Some(rid) = needs_good {
            if !consume_ground_resource_near(&mut commands, &mut ground_q, anchor, rid, 1) {
                // Cargo has not arrived yet. Drop the task so the pitch
                // dispatcher can reassign once a matching stack appears.
                finish_pitch_worker(&mut ai, &mut aq);
                continue;
            }
        }

        let world_pos = tile_to_world(anchor.0, anchor.1);
        match kind {
            BuildSiteKind::Bedroll => {
                let e = commands
                    .spawn((
                        Bed::default(),
                        Deployable::fully_packable(crate::economy::core_ids::bedroll()),
                        StructureLabel("Bedroll"),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        Indexed::new(IndexedKind::Bed),
                    ))
                    .id();
                bed_map.0.insert(anchor, e);
            }
            BuildSiteKind::Yurt => {
                commands.spawn((
                    TentShelter {
                        tier: ShelterTier::Yurt,
                    },
                    Deployable::fully_packable(crate::economy::core_ids::packed_yurt()),
                    StructureLabel("Yurt"),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ));
            }
            BuildSiteKind::Campfire => {
                // Nomad re-pitch from a packed bundle. Camp hearths are
                // always `Camp`-role; the pack-and-pitch cycle preserves
                // that because the role lives on the durable component.
                let role = crate::simulation::construction::HearthRole::Camp;
                let campfire = Campfire {
                    tier: crate::simulation::construction::HearthTier::Open,
                    role,
                };
                let label = campfire.tier.label();
                let e = commands
                    .spawn((
                        campfire,
                        StructureLabel(label),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                campfire_map.0.insert(
                    anchor,
                    crate::simulation::construction::CampfireEntry { entity: e, role },
                );
            }
            _ => {}
        }
        tile_changed.send(TileChangedEvent {
            tx: anchor.0,
            ty: anchor.1,
        });
        finish_pitch_worker(&mut ai, &mut aq);
    }
}

fn finish_pitch_worker(ai: &mut PersonAI, aq: &mut ActionQueue) {
    aq.finish_task(ai);
}

fn consume_ground_resource_near(
    commands: &mut Commands,
    ground_q: &mut Query<(Entity, &Transform, &mut GroundItem)>,
    anchor: (i32, i32),
    rid: ResourceId,
    qty: u32,
) -> bool {
    if qty == 0 {
        return true;
    }
    let mut remaining = qty;
    for (entity, transform, mut ground) in ground_q.iter_mut() {
        if ground.item.resource_id != rid || ground.qty == 0 {
            continue;
        }
        let tile = transform_tile(transform);
        if chebyshev(tile, anchor) > 2 {
            continue;
        }
        let take = remaining.min(ground.qty);
        ground.qty -= take;
        remaining -= take;
        if ground.qty == 0 {
            commands.entity(entity).despawn_recursive();
        }
        if remaining == 0 {
            return true;
        }
    }
    false
}
