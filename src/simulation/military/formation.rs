//! Multi-unit military move formations.
//!
//! When the player issues a `MilitaryMove` with more than one selected actor,
//! the single clicked tile (the *anchor*) is expanded into a compact ring of
//! per-actor *slot* tiles. Each actor routes to its own slot, so the group
//! spreads around the anchor instead of stacking on it. Single-actor moves
//! bypass the planner and behave exactly as before.
//!
//! Scope is the bug fix only: no persistent squad types, no formation pickers,
//! and no changes to `MilitaryAttack`.

use crate::collections::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::simulation::doormat::DoormatReservations;
use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
use crate::world::chunk::ChunkMap;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;

/// Per-actor marker attached during a multi-actor `MilitaryMove`. Removed
/// when the command reaches terminal status (or is superseded by a fresh
/// `MilitaryMove`). The slot tile itself rides on `PersonAI.dest_tile` via
/// `assign_task_with_routing` — we only need the metadata for inspection
/// and stale-cleanup.
#[derive(Component, Debug, Clone, Copy)]
pub struct MilitaryFormationSlot {
    pub anchor: (i32, i32),
    pub slot_index: u8,
    pub group: u32,
}

/// Monotonic source of group ids; one id per multi-actor dispatch.
#[derive(Resource, Default)]
pub struct MilitaryFormationGroupGen {
    pub next: u32,
}

impl MilitaryFormationGroupGen {
    pub fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }
}

/// Slot tile chosen for one actor in one dispatch.
#[derive(Copy, Clone, Debug)]
pub struct FormationAssignment {
    pub slot_tile: (i32, i32),
    pub group: u32,
    pub slot_index: u8,
}

/// Side-table populated by `expand_military_move_system` (Input) and read
/// by `dispatch_player_command_system` (ParallelB). Keyed by actor entity.
///
/// `Some(assignment)` → route to `slot_tile`. `None` → the planner found
/// no reachable slot for this actor; the dispatcher fails the command.
/// Actors absent from the map are single-actor `MilitaryMove`s — the
/// dispatcher uses the anchor tile directly, preserving prior behaviour.
///
/// Cleared at the start of every `expand_military_move_system` tick, so
/// stale entries don't leak across frames.
#[derive(Resource, Default)]
pub struct PendingFormationSlots {
    pub map: AHashMap<Entity, Option<FormationAssignment>>,
}

/// Walk Chebyshev rings outward from `anchor` in deterministic cardinal-
/// then-diagonal order, yielding up to `n` tiles for which `is_passable`
/// returns true. Anchor itself is slot 0 when passable. Pure; no ECS
/// access.
pub fn plan_compact_ring(
    anchor: (i32, i32),
    n: usize,
    is_passable: impl Fn((i32, i32)) -> bool,
) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(n);
    if n == 0 {
        return out;
    }
    if is_passable(anchor) {
        out.push(anchor);
        if out.len() >= n {
            return out;
        }
    }
    // Cap the search radius. n ≤ 32 typical; 16 rings hold 1024 tiles which
    // is generous headroom.
    const MAX_R: i32 = 16;
    for r in 1..=MAX_R {
        // Cardinals first (N, E, S, W) for visual clarity.
        for &(dx, dy) in &[(0, -r), (r, 0), (0, r), (-r, 0)] {
            let t = (anchor.0 + dx, anchor.1 + dy);
            if is_passable(t) {
                out.push(t);
                if out.len() >= n {
                    return out;
                }
            }
        }
        // Then the rest of the ring in (dy, dx) order, skipping cardinals
        // already emitted.
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                if dx == 0 || dy == 0 {
                    continue;
                }
                let t = (anchor.0 + dx, anchor.1 + dy);
                if is_passable(t) {
                    out.push(t);
                    if out.len() >= n {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Greedy nearest assignment from actors to slots. Actors are visited in
/// order of Chebyshev distance from `anchor` (closer first); each picks
/// the unassigned slot minimising Chebyshev distance from itself.
///
/// `actors[i] = (actor_tile, stable_tiebreak_key)`. Output `result[i]` is
/// the slot index assigned to `actors[i]`, or `None` if no slot remained.
pub fn greedy_assign(
    anchor: (i32, i32),
    actors: &[((i32, i32), usize)],
    slots: &[(i32, i32)],
) -> Vec<Option<usize>> {
    let mut order: Vec<usize> = (0..actors.len()).collect();
    order.sort_by_key(|&i| {
        let (t, key) = actors[i];
        let dx = (t.0 - anchor.0).abs();
        let dy = (t.1 - anchor.1).abs();
        (dx.max(dy), key)
    });
    let mut taken = vec![false; slots.len()];
    let mut assignment = vec![None; actors.len()];
    for &i in &order {
        let actor_tile = actors[i].0;
        let mut best: Option<(i32, usize)> = None;
        for (j, &slot) in slots.iter().enumerate() {
            if taken[j] {
                continue;
            }
            let dx = (slot.0 - actor_tile.0).abs();
            let dy = (slot.1 - actor_tile.1).abs();
            let d = dx.max(dy);
            if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, j));
            }
        }
        if let Some((_, j)) = best {
            taken[j] = true;
            assignment[i] = Some(j);
        }
    }
    assignment
}

/// Pre-dispatch: expand every multi-actor `MilitaryMove` event into per-actor
/// slot assignments via `plan_compact_ring` + `greedy_assign`. Single-actor
/// moves are skipped and dispatched as before (anchor == slot).
///
/// Runs in `SimulationSet::Input`, ahead of `dispatch_player_command_system`
/// (`ParallelB`). The map is cleared each tick so stale entries from a prior
/// dispatch never bleed through.
pub fn expand_military_move_system(
    mut reader: EventReader<PlayerCommandEvent>,
    mut pending: ResMut<PendingFormationSlots>,
    mut group_gen: ResMut<MilitaryFormationGroupGen>,
    chunk_map: Res<ChunkMap>,
    doormat: Res<DoormatReservations>,
    spatial: Res<SpatialIndex>,
    transforms: Query<&Transform>,
) {
    pending.map.clear();

    for ev in reader.read() {
        let tile = match ev.command {
            PlayerCommand::MilitaryMove { tile, .. } => tile,
            _ => continue,
        };
        if ev.actors.len() <= 1 {
            continue;
        }

        // Snapshot each actor's tile (and its position in ev.actors as a
        // deterministic tiebreak for the greedy ordering).
        let actors: Vec<((i32, i32), usize)> = ev
            .actors
            .iter()
            .enumerate()
            .filter_map(|(idx, &e)| {
                let t = transforms.get(e).ok()?;
                let tx = (t.translation.x / TILE_SIZE).floor() as i32;
                let ty = (t.translation.y / TILE_SIZE).floor() as i32;
                Some(((tx, ty), idx))
            })
            .collect();

        if actors.is_empty() {
            continue;
        }

        // Selected actors' own tiles are treated as passable so a tight
        // group can reshuffle without self-blocking.
        let mut own_tiles: AHashSet<(i32, i32)> = AHashSet::default();
        for (t, _) in &actors {
            own_tiles.insert(*t);
        }

        let is_passable = |t: (i32, i32)| -> bool {
            if own_tiles.contains(&t) {
                return true;
            }
            if !chunk_map.is_passable(t.0, t.1) {
                return false;
            }
            if doormat.is_reserved(t) {
                return false;
            }
            let z = chunk_map.surface_z_at(t.0, t.1) as i32;
            if spatial.agent_occupied(t.0, t.1, z) {
                return false;
            }
            true
        };

        let slots = plan_compact_ring(tile, actors.len(), is_passable);
        if slots.is_empty() {
            // Anchor unreachable for every actor. Mark every actor None so
            // dispatch fails them consistently.
            for &(_, idx) in &actors {
                pending.map.insert(ev.actors[idx], None);
            }
            continue;
        }

        let group = group_gen.allocate();
        let assigns = greedy_assign(tile, &actors, &slots);
        for (i, slot_idx_opt) in assigns.iter().enumerate() {
            let actor_pos = actors[i].1;
            let actor = ev.actors[actor_pos];
            match slot_idx_opt {
                Some(slot_idx) => {
                    pending.map.insert(
                        actor,
                        Some(FormationAssignment {
                            slot_tile: slots[*slot_idx],
                            group,
                            slot_index: *slot_idx as u8,
                        }),
                    );
                }
                None => {
                    pending.map.insert(actor, None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_passable(_: (i32, i32)) -> bool {
        true
    }

    #[test]
    fn plan_compact_ring_n1_returns_anchor() {
        let anchor = (5, 7);
        let out = plan_compact_ring(anchor, 1, always_passable);
        assert_eq!(out, vec![anchor]);
    }

    #[test]
    fn plan_compact_ring_n9_fills_first_ring() {
        let anchor = (0, 0);
        let out = plan_compact_ring(anchor, 9, always_passable);
        assert_eq!(out.len(), 9);
        // All within Chebyshev 1 of anchor.
        for t in &out {
            let d = (t.0 - anchor.0).abs().max((t.1 - anchor.1).abs());
            assert!(d <= 1, "tile {:?} not within ring 1", t);
        }
        // Uniqueness.
        let set: AHashSet<_> = out.iter().copied().collect();
        assert_eq!(set.len(), 9);
        // Anchor present.
        assert!(set.contains(&anchor));
    }

    #[test]
    fn plan_compact_ring_is_deterministic() {
        let anchor = (3, -2);
        let a = plan_compact_ring(anchor, 9, always_passable);
        let b = plan_compact_ring(anchor, 9, always_passable);
        assert_eq!(a, b);
    }

    #[test]
    fn plan_compact_ring_n20_monotonic_radius() {
        let anchor = (0, 0);
        let out = plan_compact_ring(anchor, 20, always_passable);
        assert_eq!(out.len(), 20);
        let set: AHashSet<_> = out.iter().copied().collect();
        assert_eq!(set.len(), 20, "duplicates emitted");
        let mut last_r = 0;
        for t in &out {
            let d = (t.0 - anchor.0).abs().max((t.1 - anchor.1).abs());
            assert!(
                d >= last_r,
                "tile {:?} (r={}) appeared after r={}",
                t,
                d,
                last_r
            );
            last_r = d;
        }
    }

    #[test]
    fn plan_compact_ring_skips_blocked_cardinals() {
        let anchor = (10, 10);
        // Every cardinal of anchor blocked; diagonals + outer ring open.
        let blocked: AHashSet<(i32, i32)> =
            [(10, 9), (10, 11), (9, 10), (11, 10)].into_iter().collect();
        let pass = |t: (i32, i32)| -> bool { !blocked.contains(&t) };
        let out = plan_compact_ring(anchor, 5, pass);
        assert_eq!(out.len(), 5);
        let set: AHashSet<_> = out.iter().copied().collect();
        assert_eq!(set.len(), 5, "duplicates emitted");
        for t in &out {
            assert!(!blocked.contains(t), "emitted blocked tile {:?}", t);
        }
    }

    #[test]
    fn plan_compact_ring_returns_short_when_insufficient_passability() {
        let anchor = (0, 0);
        // Only the anchor itself is passable.
        let pass = move |t: (i32, i32)| -> bool { t == anchor };
        let out = plan_compact_ring(anchor, 10, pass);
        assert_eq!(out, vec![anchor]);
    }

    #[test]
    fn greedy_assign_matches_actors_to_nearest_slot() {
        // Three actors clustered SW of the anchor; three slots at the
        // anchor + N + E. Each actor should grab its nearest free slot.
        let anchor = (10, 10);
        let actors = vec![
            (((9, 11), 0)),  // closest to W/SW
            (((11, 9), 1)),  // closest to E/SE
            (((10, 12), 2)), // closest to S
        ];
        let slots = vec![(10, 10), (10, 9), (11, 10)];
        let assigns = greedy_assign(anchor, &actors, &slots);
        // All actors assigned.
        for a in &assigns {
            assert!(a.is_some());
        }
        // Distinct slots.
        let mut taken: AHashSet<usize> = AHashSet::default();
        for a in &assigns {
            assert!(taken.insert(a.unwrap()));
        }
    }

    #[test]
    fn greedy_assign_is_deterministic() {
        let anchor = (0, 0);
        let actors = vec![(((-2, 0), 0)), (((2, 0), 1)), (((0, -2), 2))];
        let slots = vec![(-1, 0), (1, 0), (0, -1)];
        let a = greedy_assign(anchor, &actors, &slots);
        let b = greedy_assign(anchor, &actors, &slots);
        assert_eq!(a, b);
    }

    #[test]
    fn greedy_assign_short_slot_list_leaves_overflow_unassigned() {
        let anchor = (0, 0);
        let actors = vec![(((0, 0), 0)), (((1, 0), 1)), (((2, 0), 2))];
        // Only one slot; two actors should miss out.
        let slots = vec![(0, 0)];
        let assigns = greedy_assign(anchor, &actors, &slots);
        let assigned: usize = assigns.iter().filter(|a| a.is_some()).count();
        assert_eq!(assigned, 1);
    }
}
