use ahash::AHashMap;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct NetId(pub u32);

impl NetId {
    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Component, Copy, Clone, Debug)]
pub struct Networked(pub NetId);

#[derive(Component, Copy, Clone, Debug, Default)]
pub struct NeedsNetId;

#[derive(Resource, Default)]
pub struct NetIdMap {
    next: u32,
    entity_to_net: AHashMap<Entity, NetId>,
    net_to_entity: AHashMap<NetId, Entity>,
}

impl NetIdMap {
    pub fn alloc(&mut self, entity: Entity) -> NetId {
        if let Some(existing) = self.entity_to_net.get(&entity) {
            return *existing;
        }
        let id = NetId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("NetId u32 space exhausted within session");
        self.entity_to_net.insert(entity, id);
        self.net_to_entity.insert(id, entity);
        id
    }

    pub fn release(&mut self, entity: Entity) -> Option<NetId> {
        let id = self.entity_to_net.remove(&entity)?;
        self.net_to_entity.remove(&id);
        Some(id)
    }

    pub fn entity_of(&self, id: NetId) -> Option<Entity> {
        self.net_to_entity.get(&id).copied()
    }

    pub fn net_id_of(&self, entity: Entity) -> Option<NetId> {
        self.entity_to_net.get(&entity).copied()
    }

    pub fn len(&self) -> usize {
        self.entity_to_net.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entity_to_net.is_empty()
    }

    /// Look up an entity's `NetId`, allocating a fresh one and attaching
    /// `Networked` immediately if absent. Use this at UI / command-build
    /// sites that need a serializable handle right now and can't wait for
    /// the deferred `assign_net_ids_system` pass.
    pub fn lookup_or_alloc(&mut self, entity: Entity, commands: &mut Commands) -> NetId {
        if let Some(id) = self.net_id_of(entity) {
            return id;
        }
        let id = self.alloc(entity);
        commands.entity(entity).insert(Networked(id));
        id
    }
}

pub fn assign_net_ids_system(
    mut commands: Commands,
    mut map: ResMut<NetIdMap>,
    pending: Query<Entity, (With<NeedsNetId>, Without<Networked>)>,
) {
    for entity in &pending {
        let id = map.alloc(entity);
        commands
            .entity(entity)
            .insert(Networked(id))
            .remove::<NeedsNetId>();
    }
}

pub fn release_net_ids_on_despawn(
    mut map: ResMut<NetIdMap>,
    mut removed: RemovedComponents<Networked>,
) {
    for entity in removed.read() {
        map.release(entity);
    }
}

pub struct NetIdPlugin;

impl Plugin for NetIdPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetIdMap>().add_systems(
            PostUpdate,
            (assign_net_ids_system, release_net_ids_on_despawn).chain(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> App {
        let mut app = App::new();
        app.add_plugins(NetIdPlugin);
        app
    }

    #[test]
    fn assigns_net_id_to_marker_entities() {
        let mut app = make_app();
        let entity = app.world_mut().spawn(NeedsNetId).id();
        app.update();

        let networked = app.world().entity(entity).get::<Networked>().copied();
        assert!(networked.is_some(), "entity should have Networked after PostUpdate");
        let id = networked.unwrap().0;

        let map = app.world().resource::<NetIdMap>();
        assert_eq!(map.entity_of(id), Some(entity));
        assert_eq!(map.net_id_of(entity), Some(id));
        assert!(
            app.world().entity(entity).get::<NeedsNetId>().is_none(),
            "marker should be removed once assigned"
        );
    }

    #[test]
    fn monotonic_ids_no_reuse_on_release() {
        let mut app = make_app();

        let a = app.world_mut().spawn(NeedsNetId).id();
        app.update();
        let a_id = app.world().entity(a).get::<Networked>().unwrap().0;

        app.world_mut().entity_mut(a).despawn();
        app.update(); // release runs

        let b = app.world_mut().spawn(NeedsNetId).id();
        app.update();
        let b_id = app.world().entity(b).get::<Networked>().unwrap().0;

        assert_ne!(a_id, b_id, "released NetIds must not be reused in-session");
        assert!(b_id.raw() > a_id.raw(), "ids should be monotonic");

        let map = app.world().resource::<NetIdMap>();
        assert_eq!(map.entity_of(a_id), None, "released id maps to nothing");
    }

    #[test]
    fn alloc_is_idempotent_for_same_entity() {
        let mut map = NetIdMap::default();
        let e = Entity::from_raw(42);
        let id1 = map.alloc(e);
        let id2 = map.alloc(e);
        assert_eq!(id1, id2);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn net_id_round_trips_through_serde() {
        let id = NetId(0xDEAD_BEEF);
        let bytes = bincode::serialize(&id).unwrap();
        let back: NetId = bincode::deserialize(&bytes).unwrap();
        assert_eq!(id, back);
    }
}
