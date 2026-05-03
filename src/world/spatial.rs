use ahash::AHashMap;
use bevy::ecs::component::ComponentId;
use bevy::ecs::world::DeferredWorld;
use bevy::prelude::*;

/// Maps (tile_x, tile_y) -> list of entities occupying that tile.
/// Maintained incrementally by `sync_indexed_after_move_system` (move/spawn) and
/// the `Indexed` component's `on_remove` hook (despawn).
#[derive(Resource, Default)]
pub struct SpatialIndex {
    /// All live entities by 2-D tile position.
    pub map: AHashMap<(i32, i32), Vec<Entity>>,
    /// Number of alive mobile agents (Person, Wolf, Deer, Horse) at each 3-D tile (tx, ty, tz).
    /// Used for collision prevention and occupancy checks.
    pub agent_counts: AHashMap<(i32, i32, i32), u8>,
}

/// Tag enum identifying which kind of indexed entity this is.
/// `is_mobile_agent()` controls whether the 3-D `agent_counts` map is touched.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IndexedKind {
    Person,
    Wolf,
    Deer,
    Horse,
    Plant,
    GroundItem,
    Bed,
}

impl IndexedKind {
    #[inline]
    pub fn is_mobile_agent(self) -> bool {
        matches!(self, Self::Person | Self::Wolf | Self::Deer | Self::Horse)
    }
}

/// Component attached to every entity that should appear in `SpatialIndex`.
/// `tile.0 == i32::MIN` is a sentinel meaning "never synced" — used to skip the
/// "remove from old bucket" step on first sync after spawn.
#[derive(Component, Copy, Clone, Debug)]
pub struct Indexed {
    pub kind: IndexedKind,
    pub tile: (i32, i32),
    pub z: i32,
}

impl Indexed {
    #[inline]
    pub fn new(kind: IndexedKind) -> Self {
        Self {
            kind,
            tile: (i32::MIN, 0),
            z: 0,
        }
    }
}

pub fn on_indexed_remove(mut world: DeferredWorld<'_>, entity: Entity, _: ComponentId) {
    let Some(idx) = world.get::<Indexed>(entity).copied() else {
        return;
    };
    if idx.tile.0 == i32::MIN {
        return;
    }
    let mut spatial = world.resource_mut::<SpatialIndex>();
    spatial.remove(idx.tile.0, idx.tile.1, entity);
    if idx.kind.is_mobile_agent() {
        let key = (idx.tile.0, idx.tile.1, idx.z);
        if let Some(c) = spatial.agent_counts.get_mut(&key) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                spatial.agent_counts.remove(&key);
            }
        }
    }
}

impl SpatialIndex {
    pub fn insert(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        self.map.entry((tile_x, tile_y)).or_default().push(entity);
    }

    pub fn remove(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        if let Some(v) = self.map.get_mut(&(tile_x, tile_y)) {
            v.retain(|&e| e != entity);
        }
    }

    pub fn get(&self, tile_x: i32, tile_y: i32) -> &[Entity] {
        self.map
            .get(&(tile_x, tile_y))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Returns true if any alive mobile agent occupies (tx, ty, tz).
    pub fn agent_occupied(&self, tx: i32, ty: i32, tz: i32) -> bool {
        self.agent_counts
            .get(&(tx, ty, tz))
            .map_or(false, |&c| c > 0)
    }

    /// Returns the number of agents at (tx, ty, tz).
    pub fn agent_count(&self, tx: i32, ty: i32, tz: i32) -> u8 {
        self.agent_counts.get(&(tx, ty, tz)).copied().unwrap_or(0)
    }
}
