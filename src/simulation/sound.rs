use ahash::AHashSet;
use bevy::prelude::*;
use std::collections::VecDeque;

use crate::simulation::combat::{Body, CombatTarget, DistressCallEvent, Health};
use crate::simulation::construction::DoorMap;
use crate::simulation::goals::{AgentGoal, GoalReason, RescueTarget};
use crate::simulation::line_of_sight::has_los;
use crate::simulation::lod::LodLevel;
use crate::simulation::memory::RelationshipMemory;
use crate::simulation::person::{AiState, Person, PersonAI};
use crate::simulation::plan::ActivePlan;
use crate::simulation::schedule::SimClock;
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

pub const AUDIBLE_RANGE: u8 = 10;
pub const PROXIMITY_RANGE: i32 = 5;
pub const SIGHT_RANGE: i32 = 15;

/// 4-connected BFS from `origin` at z-level `z`. The origin is always included.
/// A neighbour is reachable iff its tile is not solid earth (`TileKind::Wall`)
/// — doors, water, forests, etc. all transmit sound. BFS terminates at depth
/// `max_dist`, so the returned set never exceeds (2·max_dist+1)² tiles.
pub fn audible_tiles(
    chunk_map: &ChunkMap,
    origin: (i32, i32),
    z: i8,
    max_dist: u8,
) -> AHashSet<(i32, i32)> {
    let mut visited: AHashSet<(i32, i32)> = AHashSet::new();
    let mut frontier: VecDeque<((i32, i32), u8)> = VecDeque::new();
    visited.insert(origin);
    frontier.push_back((origin, 0));

    while let Some(((x, y), depth)) = frontier.pop_front() {
        if depth == max_dist {
            continue;
        }
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let nx = x + dx;
            let ny = y + dy;
            if visited.contains(&(nx, ny)) {
                continue;
            }
            // Solid earth blocks sound. Underground rock surfaces as Wall via
            // tile_at, which is what we want — sound does not pass through it.
            if chunk_map.tile_at(nx, ny, z as i32).kind.is_solid() {
                continue;
            }
            visited.insert((nx, ny));
            frontier.push_back(((nx, ny), depth + 1));
        }
    }

    visited
}

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Reads `DistressCallEvent`s and recruits eligible nearby Persons to defend.
/// Eligibility (any one triggers a response):
///   - Proximity:  Chebyshev distance ≤ PROXIMITY_RANGE.
///   - Sight:      distance ≤ SIGHT_RANGE AND has LOS to the victim.
///   - Audible+bond: the responder's tile is in the audible flood AND
///                   relationship affinity to the victim is positive.
///
/// Side effects on each recruited responder:
///   - clear active task / drop ActivePlan / wake sleepers (bed claim preserved)
///   - set CombatTarget to the attacker
///   - insert RescueTarget(attacker)
///   - set AgentGoal::Rescue (so the goal_update_system override engages
///     immediately and the planner picks RescueAlly on the next tick)
pub fn respond_to_distress_system(
    mut commands: Commands,
    mut events: EventReader<DistressCallEvent>,
    chunk_map: Res<ChunkMap>,
    door_map: Res<DoorMap>,
    spatial: Res<SpatialIndex>,
    clock: Res<SimClock>,
    transform_q: Query<&Transform>,
    mut responders: Query<
        (
            Entity,
            &mut PersonAI,
            &mut CombatTarget,
            &mut AgentGoal,
            &Transform,
            &LodLevel,
            Option<&RelationshipMemory>,
            Option<&Body>,
            Option<&Health>,
            Option<&mut GoalReason>,
            Option<&ActivePlan>,
        ),
        With<Person>,
    >,
) {
    for ev in events.read() {
        let audible = audible_tiles(&chunk_map, ev.tile, ev.z, AUDIBLE_RANGE);
        let bound = SIGHT_RANGE; // outermost branch radius

        // Snapshot the attacker's tile once per event so RescueTarget carries
        // a concrete destination for the planner.
        let attacker_tile = transform_q.get(ev.attacker).ok().map(|t| {
            (
                (t.translation.x / TILE_SIZE).floor() as i16,
                (t.translation.y / TILE_SIZE).floor() as i16,
            )
        });

        // Gather candidate entities once via the spatial index, then look them up
        // in the responders query.
        let mut candidates: Vec<Entity> = Vec::new();
        for dy in -bound..=bound {
            for dx in -bound..=bound {
                let tx = ev.tile.0 + dx;
                let ty = ev.tile.1 + dy;
                for &e in spatial.get(tx, ty) {
                    if e == ev.victim || e == ev.attacker {
                        continue;
                    }
                    candidates.push(e);
                }
            }
        }

        for cand in candidates {
            let Ok((
                entity,
                mut ai,
                mut combat_target,
                mut goal,
                transform,
                lod,
                rel_opt,
                body_opt,
                health_opt,
                reason_opt,
                active_plan_opt,
            )) = responders.get_mut(cand)
            else {
                continue;
            };

            if *lod == LodLevel::Dormant {
                continue;
            }

            // Don't recruit the dying.
            if let Some(b) = body_opt {
                if b.is_dead() || b.fraction() < 0.25 {
                    continue;
                }
            }
            if let Some(h) = health_opt {
                if h.is_dead() || h.fraction() < 0.25 {
                    continue;
                }
            }

            // Already engaged with someone else — leave them alone.
            if combat_target.0.is_some() {
                continue;
            }

            let entity_tile = (
                (transform.translation.x / TILE_SIZE).floor() as i32,
                (transform.translation.y / TILE_SIZE).floor() as i32,
            );
            let dist = chebyshev(entity_tile, ev.tile);

            // Eligibility tests, ordered cheap → expensive.
            let in_proximity = dist <= PROXIMITY_RANGE;
            let in_audible_flood = audible.contains(&entity_tile);
            let positive_affinity = rel_opt
                .map(|r| r.get_affinity(ev.victim) > 0)
                .unwrap_or(false);

            let in_sight = !in_proximity
                && dist <= SIGHT_RANGE
                && has_los(
                    &chunk_map,
                    &door_map,
                    (ev.tile.0, ev.tile.1, ev.z),
                    (entity_tile.0, entity_tile.1, ai.current_z),
                );

            let eligible = in_proximity || in_sight || (in_audible_flood && positive_affinity);
            if !eligible {
                continue;
            }

            // Recruit. Wake sleepers — Bed.owner stays set so the agent can
            // return after the fight. Drop any active task / plan and pivot.
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;

            combat_target.0 = Some(ev.attacker);

            // If we lost track of the attacker's transform (despawned mid-tick),
            // skip — without a destination tile the planner can't route.
            let Some(attacker_tile) = attacker_tile else {
                continue;
            };

            commands.entity(entity).insert(RescueTarget {
                attacker: ev.attacker,
                attacker_tile,
                set_tick: clock.tick,
            });

            if active_plan_opt.is_some() {
                commands.entity(entity).remove::<ActivePlan>();
            }

            *goal = AgentGoal::Rescue;
            if let Some(mut r) = reason_opt {
                r.0 = "Helping Ally";
            } else {
                commands.entity(entity).insert(GoalReason("Helping Ally"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_chunk_map(kind: TileKind, surface_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        let z = Box::new([[surface_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let k = Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let f = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(ChunkCoord(0, 0), Chunk::new(z, k, f));
        map.0.insert(
            ChunkCoord(1, 0),
            Chunk::new(
                Box::new([[surface_z; CHUNK_SIZE]; CHUNK_SIZE]),
                Box::new([[kind; CHUNK_SIZE]; CHUNK_SIZE]),
                Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]),
            ),
        );
        map
    }

    #[test]
    fn open_ground_reaches_exactly_max_dist() {
        let map = flat_chunk_map(TileKind::Grass, 0);
        let set = audible_tiles(&map, (5, 5), 0, 10);
        // Manhattan distance equal to BFS depth in 4-connected open ground.
        assert!(set.contains(&(5 + 10, 5)));
        assert!(set.contains(&(5, 5 + 10)));
        assert!(!set.contains(&(5 + 11, 5)));
    }

    #[test]
    fn sound_bends_around_a_wall_corner() {
        // Surface map; carve a Wall divider with a one-tile gap.
        let mut map = flat_chunk_map(TileKind::Grass, 0);
        // Wall along x = 10, y = 0..20, with a gap at y = 5.
        for y in 0..20 {
            if y == 5 {
                continue;
            }
            map.set_tile(
                10,
                y,
                0,
                TileData {
                    kind: TileKind::Wall,
                    ..Default::default()
                },
            );
        }
        let set = audible_tiles(&map, (5, 5), 0, 10);
        // Tile on the far side (x=12, y=5) is reachable through the gap (BFS bends).
        assert!(set.contains(&(12, 5)));
        // Tile at (12, 0) is on the far side AND requires going around the wall —
        // path length from (5,5) → (5,0)→through-gap(10,5)→(12, …): too far.
        // (Mostly here as a sanity check that the wall was actually built.)
        assert!(!set.contains(&(11, 0)));
    }

    #[test]
    fn solid_rock_blocks_sound_between_tunnels() {
        // No carving: every voxel between two underground points is Wall.
        let map = flat_chunk_map(TileKind::Stone, 5);
        let set = audible_tiles(&map, (1, 1), -4, 10);
        // The origin is included (we don't test its solidity).
        assert!(set.contains(&(1, 1)));
        // Any neighbour underground is solid rock — propagation stops immediately.
        assert!(!set.contains(&(2, 1)));
        assert!(!set.contains(&(1, 2)));
    }

    #[test]
    fn underground_tunnel_propagates_sound_along_its_length() {
        let mut map = flat_chunk_map(TileKind::Stone, 5);
        // Carve an Air tunnel at z=-4 along y=10 from x=0..=15.
        for x in 0..=15i32 {
            map.set_tile(
                x,
                10,
                -4,
                TileData {
                    kind: TileKind::Dirt,
                    ..Default::default()
                },
            );
            map.set_tile(
                x,
                10,
                -3,
                TileData {
                    kind: TileKind::Air,
                    ..Default::default()
                },
            );
        }
        // Sound at z=-3 (inside the tunnel) from (0, 10) reaches (8, 10).
        let set = audible_tiles(&map, (0, 10), -3, 10);
        assert!(set.contains(&(8, 10)));
        // Just outside the tunnel column is solid rock, blocked.
        assert!(!set.contains(&(0, 11)));
    }
}
