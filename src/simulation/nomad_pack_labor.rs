//! Part B: observable HTN slow-path for player-driven Pack Camp.
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
//! animal AI as they migrate.
//!
//! AI factions still use the synchronous `pack_camp_assets_atomic`
//! inside `nomad_migration_commit_system` so dormant LOD bands
//! continue to migrate in one tick.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::simulation::camp::CampMap;
use crate::simulation::construction::{Bed, BedMap, Campfire, CampfireMap, TentShelter};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::gather::spawn_ground_drop;
use crate::simulation::goals::AgentGoal;
use crate::simulation::lod::LodLevel;
use crate::simulation::pack_deploy::Deployable;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::typed_task::{ActionQueue, Task};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::terrain::TILE_SIZE;

/// Work ticks each `UnpitchStructure` task accumulates before the
/// structure despawns. ~2 seconds at 20 Hz feels like real labor
/// without dragging the pack-up out unbearably for a 12-shelter camp.
pub const UNPITCH_WORK_TICKS: u32 = 40;

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

/// Snapshot of a Deployable structure that needs dismantling.
struct StructureToUnpitch {
    fid: u32,
    entity: Entity,
    tile: (i32, i32),
}

/// Dispatch a `Task::UnpitchStructure` for every Deployable found
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
        let mut state: SystemState<Query<(Entity, &Transform), With<Deployable>>> =
            SystemState::new(world);
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
                (Entity, &FactionMember, &Transform, &PersonAI),
                (With<Person>, Without<Drafted>),
            >,
            Res<FactionRegistry>,
        )> = SystemState::new(world);
        let (q, registry) = state.get(world);
        let csz = CHUNK_SIZE as i32;
        let mut acc: ahash::AHashMap<u32, Vec<Worker>> = ahash::AHashMap::default();
        for (entity, member, transform, ai) in q.iter() {
            let root = registry.root_faction(member.faction_id);
            if !packs.iter().any(|(fid, _, _)| *fid == root) {
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
        Query<(
            &mut PersonAI,
            &mut ActionQueue,
            &mut AgentGoal,
        )>,
    )> = SystemState::new(world);
    let (chunk_map, chunk_graph, chunk_router, chunk_connectivity, mut q) = state.get_mut(world);
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
            Some(a.structure),
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
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
    deployable_q: Query<(&Transform, &Deployable)>,
    bed_q: Query<&Bed>,
    campfire_q: Query<&Campfire>,
    tent_q: Query<&TentShelter>,
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
        if ai.state != AiState::Working || ai.task_id != TaskKind::UnpitchStructure as u16 {
            continue;
        }
        let Some(structure) = aq.current.as_unpitch_structure() else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            aq.advance();
            continue;
        };
        // Structure gone (raced or already despawned): clean exit.
        let Ok((transform, deploy)) = deployable_q.get(structure) else {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.work_progress = 0;
            aq.advance();
            continue;
        };
        if (ai.work_progress as u32) < UNPITCH_WORK_TICKS {
            continue;
        }

        // ── Completion: stash the packed form(s) in the worker's
        // inventory if they fit; spill to ground otherwise so the
        // band can scavenge as it moves.
        let tile = transform_tile(transform);
        let mut stash_or_drop =
            |commands: &mut Commands,
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

        // ── Drop from maps if registered there.
        if bed_q.get(structure).is_ok() {
            bed_map.0.remove(&tile);
        }
        if campfire_q.get(structure).is_ok() {
            campfire_map.0.remove(&tile);
        }
        let _ = tent_q; // marker query — no map cleanup needed.

        commands.entity(structure).despawn_recursive();
        tile_changed.send(TileChangedEvent {
            tx: tile.0,
            ty: tile.1,
        });

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
        ai.work_progress = 0;
        aq.advance();
    }

    // Avoid unused-import warning when CampMap isn't actually queried;
    // its presence here documents the intended faction-scope link.
    let _ = std::marker::PhantomData::<CampMap>;
}
