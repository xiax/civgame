//! Per-worker reservations for the stand tile of an adjacent-interaction task.
//!
//! When five thirsty workers dispatch to drink at the same well in one tick,
//! `assign_task_with_routing` independently picks the Manhattan-nearest free
//! adjacent tile. Without coordination they all stamp the **same** stand tile
//! into `ai.target_tile`; on arrival, the movement-side collision-recovery has
//! to fan them out. That recovery has historically anchored on the now-blocked
//! stand tile rather than the work tile and produced silent "Working on a
//! tile chebyshev > 1 from the well" failures.
//!
//! `StandTileReservations` is the upstream fix: stamp the chosen stand tile at
//! dispatch so the next dispatcher in the same tick can't pick it. Mirrors the
//! `StorageReservations` / `DoormatReservations` / `PlantingReservations`
//! pattern — try_stake at dispatch, release at every executor exit + chain
//! cancel, daily GC backstop for the leak-shaped paths (Dormant LOD, goal
//! flip, agent despawn).
//!
//! Reservation key is `(stand_x, stand_y, stand_z)` (i32, i32, i8). One stand
//! tile, one worker; the `worker_entity` value is recorded so a release path
//! that doesn't know the tile (worker-side teardown) can still clear it via
//! `release_for_worker`.
//!
//! Wrapped in a `Mutex` like `StorageReservations` so par_iter_mut dispatchers
//! that capture this as `Res<StandTileReservations>` can mutate without a
//! `ResMut` exclusive borrow. Critical sections are a single hashmap op each.

use crate::collections::AHashMap;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::sync::Mutex;

use crate::simulation::lod::LodLevel;
use crate::simulation::person::PersonAI;
use crate::simulation::schedule::SimClock;
use crate::simulation::tasks::task_interacts_from_adjacent;
use crate::simulation::typed_task::{ActionQueue, UNEMPLOYED_TASK_KIND};

/// Bundle of routing-resource refs required by `assign_task_with_routing` after
/// the occupancy-aware-pathing change. Lets dispatchers stay under Bevy's
/// 16-system-param ceiling by accepting one `StandRouting` arg instead of three.
#[derive(SystemParam)]
pub struct StandRouting<'w> {
    pub spatial_index: Res<'w, crate::world::spatial::SpatialIndex>,
    pub stand_reservations: Res<'w, StandTileReservations>,
    pub clock: Res<'w, SimClock>,
}

/// Per-tile worker entry recorded by `StandTileReservations`.
#[derive(Clone, Copy, Debug)]
pub struct StandReservation {
    pub worker: Entity,
    pub reserved_tick: u64,
}

#[derive(Resource, Default)]
pub struct StandTileReservations {
    inner: Mutex<AHashMap<(i32, i32, i8), StandReservation>>,
}

impl StandTileReservations {
    /// Returns the worker currently holding `(tx, ty, tz)`, if any.
    pub fn holder(&self, tx: i32, ty: i32, tz: i8) -> Option<Entity> {
        self.inner
            .lock()
            .unwrap()
            .get(&(tx, ty, tz))
            .map(|r| r.worker)
    }

    /// Returns `true` if `(tx, ty, tz)` is held by anyone **other than** `worker`.
    /// An agent's own reservation never excludes itself.
    pub fn is_taken_by_other(&self, tx: i32, ty: i32, tz: i8, worker: Entity) -> bool {
        match self.inner.lock().unwrap().get(&(tx, ty, tz)) {
            Some(r) => r.worker != worker,
            None => false,
        }
    }

    /// Attempt to stake `(tx, ty, tz)` for `worker`. Returns `true` on success.
    /// If `worker` already holds the slot, returns `true` (idempotent re-stake).
    pub fn try_stake(&self, tx: i32, ty: i32, tz: i8, worker: Entity, now: u64) -> bool {
        let mut m = self.inner.lock().unwrap();
        match m.get(&(tx, ty, tz)) {
            Some(r) if r.worker == worker => true,
            Some(_) => false,
            None => {
                m.insert(
                    (tx, ty, tz),
                    StandReservation {
                        worker,
                        reserved_tick: now,
                    },
                );
                true
            }
        }
    }

    /// Drop the reservation at `(tx, ty, tz)` regardless of holder. Idempotent.
    /// Used by the movement-side recovery path that has the old tile in hand.
    pub fn release_tile(&self, tx: i32, ty: i32, tz: i8) {
        self.inner.lock().unwrap().remove(&(tx, ty, tz));
    }

    /// Drop every reservation held by `worker`. The canonical worker-side
    /// release — executor exits, chain cancels, despawn hooks all funnel here
    /// because they don't always know the stand tile (e.g. goal flip before
    /// arrival, where `ai.target_tile` may have changed twice).
    pub fn release_for_worker(&self, worker: Entity) {
        self.inner.lock().unwrap().retain(|_, r| r.worker != worker);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// Snapshot all (tile, reservation) entries — for tests and debug only.
    pub fn snapshot(&self) -> Vec<((i32, i32, i8), StandReservation)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect()
    }
}

/// Daily GC backstop for `StandTileReservations`. Drops any reservation whose
/// holder (a) no longer exists, (b) is Dormant LOD, (c) no longer carries an
/// adjacent-interaction task, or (d) is older than `RESERVATION_MAX_AGE_TICKS`.
///
/// The per-exit `release_for_worker` calls cover the success and most failure
/// paths; this sweep handles the leak-shaped tails (goal flip mid-walk drops
/// chain without notifying us, agent despawned mid-task, etc.). Mirrors
/// `planting_reservation_gc_system`.
pub fn stand_reservation_gc_system(
    clock: Res<SimClock>,
    reservations: Res<StandTileReservations>,
    q: Query<(&PersonAI, &ActionQueue, &LodLevel)>,
) {
    const RESERVATION_MAX_AGE_TICKS: u64 = crate::world::seasons::TICKS_PER_DAY as u64;
    let now = clock.tick;
    if now % RESERVATION_MAX_AGE_TICKS != 0 {
        return;
    }
    let mut m = reservations.inner.lock().unwrap();
    m.retain(|_tile, r| {
        if now.saturating_sub(r.reserved_tick) > RESERVATION_MAX_AGE_TICKS {
            return false;
        }
        match q.get(r.worker) {
            Err(_) => false,
            Ok((_ai, aq, lod)) => {
                if matches!(lod, LodLevel::Dormant) {
                    return false;
                }
                let kind = aq.current_task_kind();
                if kind == UNEMPLOYED_TASK_KIND {
                    return false;
                }
                task_interacts_from_adjacent(kind)
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::entity::Entity;

    fn ent(id: u32) -> Entity {
        Entity::from_raw(id)
    }

    #[test]
    fn try_stake_blocks_second_holder() {
        let r = StandTileReservations::default();
        assert!(r.try_stake(3, 4, 0, ent(1), 100));
        assert!(!r.try_stake(3, 4, 0, ent(2), 101));
        assert_eq!(r.holder(3, 4, 0), Some(ent(1)));
    }

    #[test]
    fn restake_by_same_worker_is_idempotent() {
        let r = StandTileReservations::default();
        assert!(r.try_stake(3, 4, 0, ent(1), 100));
        assert!(r.try_stake(3, 4, 0, ent(1), 200));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn is_taken_by_other_self_tolerant() {
        let r = StandTileReservations::default();
        r.try_stake(3, 4, 0, ent(1), 100);
        assert!(r.is_taken_by_other(3, 4, 0, ent(2)));
        assert!(!r.is_taken_by_other(3, 4, 0, ent(1)));
    }

    #[test]
    fn release_for_worker_drops_all_entries() {
        let r = StandTileReservations::default();
        r.try_stake(3, 4, 0, ent(1), 100);
        r.try_stake(5, 6, 0, ent(1), 100);
        r.try_stake(7, 8, 0, ent(2), 100);
        r.release_for_worker(ent(1));
        assert_eq!(r.len(), 1);
        assert_eq!(r.holder(7, 8, 0), Some(ent(2)));
    }

    #[test]
    fn release_tile_drops_specific_slot() {
        let r = StandTileReservations::default();
        r.try_stake(3, 4, 0, ent(1), 100);
        r.release_tile(3, 4, 0);
        assert!(r.is_empty());
    }
}
