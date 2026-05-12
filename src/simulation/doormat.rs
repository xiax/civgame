//! Doormat reservations — the 1-tile cardinal-outside neighbour of every Door.
//!
//! Reserved at door placement (both chief-built blueprints and game-start
//! seeded houses), honoured by every footprint / wall / palisade / farmland
//! placement system so neighbour walls can never block a door. The
//! `RoadCarveQueue` is also pushed so the doormat tile is carved as `Road`
//! and the door connects to the carved street spine.
//!
//! Lifecycle: the `Door` `on_remove` component hook frees the doormat entry
//! so demolished or evicted buildings release their reservation.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::land::TileEdge;

#[derive(Clone, Copy, Debug)]
pub struct DoormatEntry {
    /// The Door entity this doormat belongs to. Used by the `on_remove`
    /// hook to identify which doormat to drop.
    pub owner_door: Entity,
    /// The door's own tile (one step inward from the doormat tile).
    pub door_tile: (i32, i32),
    /// Cardinal the door opens onto — also the direction from the door tile
    /// to the doormat tile.
    pub dir: TileEdge,
}

/// Tile → reservation. Hot lookup for placement systems; small set (≤ one
/// per building) so the AHashMap stays cheap.
#[derive(Resource, Default)]
pub struct DoormatReservations(pub AHashMap<(i32, i32), DoormatEntry>);

impl DoormatReservations {
    pub fn is_reserved(&self, tile: (i32, i32)) -> bool {
        self.0.contains_key(&tile)
    }
}

/// `Door::on_remove` hook: walk the reservation map and drop any entry whose
/// `owner_door` matches the entity being removed. Registered in
/// `SimulationPlugin::build` next to the `JobEscrow` hook.
pub fn release_doormat_on_door_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: bevy::prelude::Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(mut reservations) = world.get_resource_mut::<DoormatReservations>() else {
        return;
    };
    reservations.0.retain(|_, entry| entry.owner_door != entity);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    #[test]
    fn is_reserved_returns_true_for_inserted_tile() {
        let mut r = DoormatReservations::default();
        let dummy = Entity::from_raw(7);
        r.0.insert(
            (3, 4),
            DoormatEntry {
                owner_door: dummy,
                door_tile: (2, 4),
                dir: TileEdge::East,
            },
        );
        assert!(r.is_reserved((3, 4)));
        assert!(!r.is_reserved((3, 5)));
    }
}
