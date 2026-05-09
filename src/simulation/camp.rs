//! Camp entity for nomadic factions (P1b of the capabilities/storage-parity
//! refactor).
//!
//! A `Camp` is the lightweight nomadic counterpart to `Settlement`: it
//! carries a `SettlementMarket` and a civic `treasury` so nomadic
//! factions get the same market + treasury infrastructure as settled
//! ones, but it has no plots, no zones, no `StreetSpine`, and no
//! `peak_population` — all the spatial/civic machinery that's
//! meaningless for a band that migrates seasonally.
//!
//! Settled factions don't get a Camp. Their Settlement(s) are the
//! economic node(s); multi-settlement factions naturally have multiple
//! Settlement entities.
//!
//! Lifecycle (P1b minimal): one Camp per nomadic faction, spawned by
//! `auto_found_default_camps_system` at the faction's `home_tile`.
//! Phase 3 lifecycle events will move/destroy Camps on
//! `Migrate`/`Abandon`/`SwitchArchetype` directly.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::market::SettlementMarket;
use crate::simulation::faction::{FactionRegistry, SOLO};

/// One Camp entity per nomadic faction. Mirrors the economic surface
/// of `Settlement` (market + civic treasury) without any of the
/// spatial machinery.
#[derive(Component, Clone, Debug)]
pub struct Camp {
    /// Faction that owns this camp. A nomadic faction has at most one
    /// live Camp; on migration the same Camp's `home_tile` is mutated
    /// in place (Phase 3 lifecycle event).
    pub owner_faction: u32,
    /// Current camp anchor. Mirrors `FactionData.home_tile` for
    /// nomads; updated when the faction migrates.
    pub home_tile: (i32, i32),
    pub founding_tick: u64,
    /// Civic treasury for camp-local public works (parallel to
    /// `Settlement.treasury`). Stays at 0 until Phase 4+ ramps up
    /// nomadic chief postings funded from civic wealth.
    pub treasury: f32,
    /// Per-camp market (parallel to `Settlement.market`). Lets
    /// nomadic agents trade locally instead of falling through to the
    /// global `Market` resource.
    pub market: SettlementMarket,
}

/// Resource indexing every live `Camp` entity by owner faction.
/// Parallel to `SettlementMap`, but per-faction-singleton (a nomadic
/// faction has one camp at a time — multiple camps would be a
/// multi-band split, deferred).
#[derive(Resource, Default)]
pub struct CampMap {
    pub by_faction: AHashMap<u32, Entity>,
}

impl CampMap {
    pub fn entity_for_faction(&self, faction_id: u32) -> Option<Entity> {
        self.by_faction.get(&faction_id).copied()
    }
}

/// Resolved economic node for a faction. Settled factions resolve to
/// their first `Settlement`; nomadic factions resolve to their `Camp`.
#[derive(Copy, Clone, Debug)]
pub enum MarketNodeRef {
    Settlement(Entity),
    Camp(Entity),
}

/// Find the economic node for `faction_id`. Prefers Settlement (since
/// settled factions can have multiple settlements and Camp is
/// nomadic-only), falls back to Camp.
///
/// Multi-settlement note: returns the *first* Settlement. Routing
/// nearest-by-position is the consumer's job; this helper handles the
/// "do we have a Settlement vs a Camp" axis only.
pub fn faction_market_node(
    settlement_map: &crate::simulation::settlement::SettlementMap,
    camp_map: &CampMap,
    faction_id: u32,
) -> Option<MarketNodeRef> {
    if faction_id == SOLO {
        return None;
    }
    if let Some(sid) = settlement_map.first_for_faction(faction_id) {
        if let Some(&e) = settlement_map.by_id.get(&sid) {
            return Some(MarketNodeRef::Settlement(e));
        }
    }
    camp_map
        .entity_for_faction(faction_id)
        .map(MarketNodeRef::Camp)
}

/// Auto-found one Camp per Camp-mode faction that doesn't have one
/// yet. Mirrors `auto_found_default_settlements_system`'s shape;
/// gates on `caps.settlement.is_camp()` instead of `is_full_settlement()`.
pub fn auto_found_default_camps_system(
    mut commands: Commands,
    mut map: ResMut<CampMap>,
    registry: Res<FactionRegistry>,
    clock: Res<crate::simulation::schedule::SimClock>,
) {
    for (faction_id, data) in registry.factions.iter() {
        if *faction_id == SOLO {
            continue;
        }
        if map.by_faction.contains_key(faction_id) {
            continue;
        }
        if !data.caps.settlement.is_camp() {
            continue;
        }
        let entity = commands
            .spawn(Camp {
                owner_faction: *faction_id,
                home_tile: data.home_tile,
                founding_tick: clock.tick,
                treasury: 0.0,
                market: SettlementMarket::default(),
            })
            .id();
        map.by_faction.insert(*faction_id, entity);
    }
}

// `camp_price_update_system` lives in `economy/market.rs` alongside
// `settlement_price_update_system` so both share the
// `PRICE_UPDATE_INTERVAL` cadence + `EconomicMode::Command` short-circuit.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::settlement::{Settlement, SettlementId, SettlementMap};

    /// Helper to construct a minimal `World` with the resources
    /// `faction_market_node` consults. Inserts the maps and faction
    /// registry so callers can populate them directly.
    fn world_with_maps() -> World {
        let mut w = World::new();
        w.insert_resource(SettlementMap::default());
        w.insert_resource(CampMap::default());
        w
    }

    #[test]
    fn settled_faction_resolves_to_settlement_not_camp() {
        let mut w = world_with_maps();
        let s_entity = w.spawn(()).id();
        let mut sm = w.resource_mut::<SettlementMap>();
        sm.register(SettlementId(0), s_entity, (0, 0), 7);
        sm.next_id = 1;
        let resolved =
            faction_market_node(&w.resource::<SettlementMap>(), &w.resource::<CampMap>(), 7);
        match resolved {
            Some(MarketNodeRef::Settlement(e)) => assert_eq!(e, s_entity),
            other => panic!("expected Settlement, got {:?}", other),
        }
    }

    #[test]
    fn nomadic_faction_resolves_to_camp() {
        let mut w = world_with_maps();
        let c_entity = w.spawn(()).id();
        w.resource_mut::<CampMap>().by_faction.insert(9, c_entity);
        let resolved =
            faction_market_node(&w.resource::<SettlementMap>(), &w.resource::<CampMap>(), 9);
        match resolved {
            Some(MarketNodeRef::Camp(e)) => assert_eq!(e, c_entity),
            other => panic!("expected Camp, got {:?}", other),
        }
    }

    #[test]
    fn solo_resolves_to_none() {
        let w = world_with_maps();
        let resolved = faction_market_node(
            &w.resource::<SettlementMap>(),
            &w.resource::<CampMap>(),
            crate::simulation::faction::SOLO,
        );
        assert!(resolved.is_none());
    }

    /// P1b regression invariant: a multi-settlement faction must keep
    /// each Settlement's market independent. The helper returns the
    /// *first* registered Settlement; both entities remain queryable
    /// with their own per-settlement `SettlementMarket` state.
    #[test]
    fn multi_settlement_faction_preserves_per_node_markets() {
        let mut w = world_with_maps();
        let s1 = w
            .spawn(Settlement {
                id: SettlementId(0),
                owner_faction: 5,
                market_tile: (0, 0),
                founding_tick: 0,
                name: "A".into(),
                treasury: 0.0,
                market: crate::economy::market::SettlementMarket::default(),
                peak_population: 0,
            })
            .id();
        let s2 = w
            .spawn(Settlement {
                id: SettlementId(1),
                owner_faction: 5,
                market_tile: (80, 80),
                founding_tick: 0,
                name: "B".into(),
                treasury: 0.0,
                market: crate::economy::market::SettlementMarket::default(),
                peak_population: 0,
            })
            .id();
        {
            let mut sm = w.resource_mut::<SettlementMap>();
            sm.register(SettlementId(0), s1, (0, 0), 5);
            sm.register(SettlementId(1), s2, (2, 2), 5);
            sm.next_id = 2;
        }
        // Mutate the second Settlement's market so we can observe it
        // didn't leak into the first.
        w.get_mut::<Settlement>(s2)
            .unwrap()
            .market
            .set_stock(crate::economy::core_ids::wood(), 42.0);

        let s1_view = w.get::<Settlement>(s1).unwrap();
        let s2_view = w.get::<Settlement>(s2).unwrap();
        assert_eq!(s1_view.market.stock_of(crate::economy::core_ids::wood()), 0.0);
        assert_eq!(s2_view.market.stock_of(crate::economy::core_ids::wood()), 42.0);

        // Helper picks the first one registered.
        let resolved =
            faction_market_node(&w.resource::<SettlementMap>(), &w.resource::<CampMap>(), 5);
        match resolved {
            Some(MarketNodeRef::Settlement(e)) => assert_eq!(e, s1),
            other => panic!("expected Settlement(s1), got {:?}", other),
        }
    }
}
