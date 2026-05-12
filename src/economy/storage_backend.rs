//! Storage backend abstraction (P2a of the capabilities/storage-parity refactor).
//!
//! Faction storage today comes in three flavours expressed by
//! `FactionCapabilities::storage`:
//!
//! - `FactionTile` — settled. Goods sit on `FactionStorageTile` ground
//!   items at a fixed home tile. Withdraws walk to the nearest tile
//!   carrying the resource.
//! - `MemberPool` — nomadic. Goods live in band-member inventories
//!   (`EconomicAgent.inventory`). Withdraws walk to a teammate carrying
//!   the resource.
//! - `Hybrid` — caravan-style. Tile-first, member-pool fallback.
//! - `CaravanBundles` — deferred (Phase 6+); for pack-animal `PackBundle`s.
//!
//! `compute_faction_storage_system` already gates each pass on
//! `caps.storage` (from P1a). This module adds the typed enums HTN
//! consumes when picking *where* to withdraw from / deposit to, plus
//! per-backend `nearest_withdraw` / `nearest_deposit` helpers that
//! mirror the existing dispatcher patterns at `htn.rs:4401-4405`.
//!
//! P2a scope: types + helpers only. The HTN task variant
//! (`Task::WalkAndTakeFromMember`) and the executor path that lets a
//! nomadic agent transfer goods from a teammate's inventory land in
//! P2b — the path is currently unreachable because nomadic factions
//! have `caps.posting.is_disabled()` and never emit chief withdraw
//! requests.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::archetype::StorageBackendKind;
use crate::simulation::faction::{FactionMember, StorageTileMap};

/// Where to withdraw a resource from. HTN consumes this enum in
/// place of a raw `(tile, rid, qty)` tuple so member-pool / pack-bundle
/// sources can route through the same method machinery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawSource {
    /// `FactionTile` / `Hybrid`-tile path: `Task::WalkTo { tile } →
    /// Task::WithdrawMaterial { rid, qty }` (existing flow).
    GroundTile { pos: (i32, i32) },
    /// `MemberPool` / `Hybrid`-member path: `Task::WalkTo { member.tile }
    /// → Task::WalkAndTakeFromMember { target, rid, qty }` (P2b path).
    MemberHands { entity: Entity },
    /// Future caravan path; placeholder for `CaravanBundles`.
    PackBundle { entity: Entity },
}

/// Where to deposit a resource. Symmetric to `WithdrawSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositTarget {
    GroundTile { pos: (i32, i32) },
    MemberHands { entity: Entity },
    PackBundle { entity: Entity },
}

/// Pick the nearest withdraw source for `(faction_id, resource_id)`,
/// near `from`. Backend-aware: `FactionTile` walks
/// `StorageTileMap.by_faction` (mirrors htn.rs:4401-4405);
/// `MemberPool` walks the supplied `(member, agent, tile)` iterator
/// for the same faction; `Hybrid` falls back from tile to member.
///
/// Caller passes the member iterator because Bevy queries are
/// SystemParams: this helper stays query-shape-agnostic.
///
/// Multi-settlement: `StorageTileMap.by_faction[fid]` is already a
/// flat list of every faction-owned tile across every Settlement, and
/// we pick the chebyshev-min distance to `from` — so a faction with
/// two settlements 80 tiles apart routes the agent to the closer one
/// naturally.
pub fn nearest_withdraw<'a, I>(
    kind: StorageBackendKind,
    faction_id: u32,
    rid: ResourceId,
    from: (i32, i32),
    storage_tile_map: &StorageTileMap,
    tile_stock_at: impl Fn((i32, i32), ResourceId) -> u32,
    members: I,
) -> Option<WithdrawSource>
where
    I: IntoIterator<Item = (Entity, &'a FactionMember, (i32, i32), u32)>,
{
    let try_tile = || {
        let tiles = storage_tile_map.by_faction.get(&faction_id)?;
        tiles
            .iter()
            .copied()
            .filter(|t| tile_stock_at(*t, rid) > 0)
            .min_by_key(|&(tx, ty)| (tx - from.0).abs() + (ty - from.1).abs())
            .map(|t| WithdrawSource::GroundTile { pos: t })
    };
    let try_member = || -> Option<WithdrawSource> {
        members
            .into_iter()
            .filter(|(_e, fm, _tile, qty)| fm.faction_id == faction_id && *qty > 0)
            .min_by_key(|(_e, _fm, tile, _qty)| (tile.0 - from.0).abs() + (tile.1 - from.1).abs())
            .map(|(e, _, _, _)| WithdrawSource::MemberHands { entity: e })
    };
    match kind {
        StorageBackendKind::FactionTile => try_tile(),
        StorageBackendKind::MemberPool => try_member(),
        StorageBackendKind::Hybrid => try_tile().or_else(try_member),
        // PackBundles deferred. Today nothing routes through this arm
        // (no archetype carries it); future work in Phase 6+.
        StorageBackendKind::CaravanBundles => None,
    }
}

/// Pick the nearest deposit target for `(faction_id, resource_id)`,
/// near `from`. Settled = nearest `FactionStorageTile` (capacity
/// check is the caller's job — ground items can stack indefinitely
/// today, so any tile accepts deposits). Nomadic = nearest faction
/// member with inventory headroom.
pub fn nearest_deposit<'a, I>(
    kind: StorageBackendKind,
    faction_id: u32,
    from: (i32, i32),
    storage_tile_map: &StorageTileMap,
    members: I,
) -> Option<DepositTarget>
where
    I: IntoIterator<Item = (Entity, &'a FactionMember, (i32, i32), bool)>,
{
    let try_tile = || {
        storage_tile_map
            .nearest_for_faction(faction_id, from)
            .map(|pos| DepositTarget::GroundTile { pos })
    };
    let try_member = || -> Option<DepositTarget> {
        members
            .into_iter()
            .filter(|(_e, fm, _tile, has_room)| fm.faction_id == faction_id && *has_room)
            .min_by_key(|(_e, _fm, tile, _)| (tile.0 - from.0).abs() + (tile.1 - from.1).abs())
            .map(|(e, _, _, _)| DepositTarget::MemberHands { entity: e })
    };
    match kind {
        StorageBackendKind::FactionTile => try_tile(),
        StorageBackendKind::MemberPool => try_member(),
        StorageBackendKind::Hybrid => try_tile().or_else(try_member),
        StorageBackendKind::CaravanBundles => None,
    }
}

/// Sum the rolled-up `(rid → qty)` totals for a faction by backend.
/// Mirrors `compute_faction_storage_system`'s two passes — but called
/// in isolation (e.g. tests, debug panels) without re-running the
/// system. The system itself stays the source of truth for
/// `FactionData.storage.totals` cache invalidation; this is a
/// stateless helper for verification.
pub fn rollup_for_kind<'m, GI, MI>(
    kind: StorageBackendKind,
    faction_id: u32,
    storage_tile_map: &StorageTileMap,
    ground_items: GI,
    members: MI,
) -> AHashMap<ResourceId, u32>
where
    GI: IntoIterator<Item = ((i32, i32), ResourceId, u32)>,
    MI: IntoIterator<Item = (&'m FactionMember, &'m crate::economy::agent::EconomicAgent)>,
{
    let mut totals: AHashMap<ResourceId, u32> = AHashMap::new();
    let pulls_tile = matches!(
        kind,
        StorageBackendKind::FactionTile | StorageBackendKind::Hybrid
    );
    let pulls_members = matches!(
        kind,
        StorageBackendKind::MemberPool | StorageBackendKind::Hybrid
    );
    if pulls_tile {
        for (tile, rid, qty) in ground_items {
            if storage_tile_map.tiles.get(&tile).copied() == Some(faction_id) {
                *totals.entry(rid).or_insert(0) += qty;
            }
        }
    }
    if pulls_members {
        for (fm, agent) in members {
            if fm.faction_id != faction_id {
                continue;
            }
            for (item, qty) in agent.inventory.iter() {
                if *qty == 0 {
                    continue;
                }
                *totals.entry(item.resource_id).or_insert(0) += *qty;
            }
        }
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::agent::EconomicAgent;
    use crate::economy::core_ids;
    use crate::economy::item::Item;
    use crate::economy::resource_catalog::load_resource_catalog;
    use bevy::prelude::*;

    fn install_catalog() {
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat);
    }

    fn make_storage_tile_map(faction_id: u32, tiles: &[(i32, i32)]) -> StorageTileMap {
        let mut map = StorageTileMap::default();
        for &t in tiles {
            map.tiles.insert(t, faction_id);
            map.by_faction.entry(faction_id).or_default().push(t);
        }
        map
    }

    /// P2a invariant: tile-stocked goods land in `totals` for FactionTile,
    /// member-stocked goods land for MemberPool. Hybrid sees both.
    #[test]
    fn rollup_invariant_each_backend() {
        install_catalog();
        let wood = core_ids::wood();
        let stone = core_ids::stone();
        let map = make_storage_tile_map(7, &[(0, 0)]);
        // Tile pass: 5 wood at (0,0) belonging to faction 7.
        let ground = vec![((0, 0), wood, 5u32)];
        // Member pass: one member of faction 7 carrying 3 stone.
        let mut agent = EconomicAgent::default();
        agent.inventory[0] = (Item::new_commodity(stone), 3);
        let fm = FactionMember {
            faction_id: 7,
            ..FactionMember::default()
        };

        let tile_only = rollup_for_kind(
            StorageBackendKind::FactionTile,
            7,
            &map,
            ground.iter().copied(),
            std::iter::once((&fm, &agent)),
        );
        assert_eq!(tile_only.get(&wood).copied(), Some(5));
        assert!(
            !tile_only.contains_key(&stone),
            "FactionTile must not pool members"
        );

        let member_only = rollup_for_kind(
            StorageBackendKind::MemberPool,
            7,
            &map,
            ground.iter().copied(),
            std::iter::once((&fm, &agent)),
        );
        assert_eq!(member_only.get(&stone).copied(), Some(3));
        assert!(
            !member_only.contains_key(&wood),
            "MemberPool must not read tile ground items"
        );

        let hybrid = rollup_for_kind(
            StorageBackendKind::Hybrid,
            7,
            &map,
            ground.iter().copied(),
            std::iter::once((&fm, &agent)),
        );
        assert_eq!(hybrid.get(&wood).copied(), Some(5));
        assert_eq!(hybrid.get(&stone).copied(), Some(3));
    }

    /// P2 multi-settlement regression: nearest_withdraw returns the
    /// closer of two same-faction Settlement tiles. Today's HTN
    /// dispatcher pattern (htn.rs:4401-4405) does this iteratively;
    /// the helper preserves it for the future migration.
    #[test]
    fn nearest_withdraw_picks_closer_settlement() {
        install_catalog();
        let wood = core_ids::wood();
        let map = make_storage_tile_map(5, &[(0, 0), (80, 80)]);
        // Both tiles hold wood.
        let pick = nearest_withdraw(
            StorageBackendKind::FactionTile,
            5,
            wood,
            (75, 75),
            &map,
            |_, _| 1, // nonzero stock everywhere
            std::iter::empty::<(Entity, &FactionMember, (i32, i32), u32)>(),
        );
        assert_eq!(pick, Some(WithdrawSource::GroundTile { pos: (80, 80) }));
    }

    /// P2 nomadic regression: nearest_withdraw picks the closest
    /// member carrying `rid` for MemberPool factions.
    #[test]
    fn member_pool_withdraw_picks_nearest_member() {
        install_catalog();
        let wood = core_ids::wood();
        let map = StorageTileMap::default();
        let alice = Entity::from_raw(1);
        let bob = Entity::from_raw(2);
        let fm = FactionMember {
            faction_id: 9,
            ..FactionMember::default()
        };
        // alice at (0,0) with 5 wood; bob at (10,10) with 5 wood. Agent at (8,8) → bob is closer.
        let members: Vec<(Entity, &FactionMember, (i32, i32), u32)> =
            vec![(alice, &fm, (0, 0), 5), (bob, &fm, (10, 10), 5)];
        let pick = nearest_withdraw(
            StorageBackendKind::MemberPool,
            9,
            wood,
            (8, 8),
            &map,
            |_, _| 0,
            members.into_iter(),
        );
        assert_eq!(pick, Some(WithdrawSource::MemberHands { entity: bob }));
    }

    /// P2 hybrid: tile path wins when tile stock is available,
    /// otherwise falls back to member-hands.
    #[test]
    fn hybrid_withdraw_prefers_tile_then_falls_back() {
        install_catalog();
        let wood = core_ids::wood();
        let map = make_storage_tile_map(11, &[(0, 0)]);
        let alice = Entity::from_raw(1);
        let fm = FactionMember {
            faction_id: 11,
            ..FactionMember::default()
        };
        let members: Vec<(Entity, &FactionMember, (i32, i32), u32)> = vec![(alice, &fm, (5, 5), 3)];

        // Tile has stock → tile wins.
        let pick = nearest_withdraw(
            StorageBackendKind::Hybrid,
            11,
            wood,
            (5, 5),
            &map,
            |t, _| if t == (0, 0) { 7 } else { 0 },
            members.iter().copied(),
        );
        assert_eq!(pick, Some(WithdrawSource::GroundTile { pos: (0, 0) }));

        // Tile empty → member fallback.
        let pick = nearest_withdraw(
            StorageBackendKind::Hybrid,
            11,
            wood,
            (5, 5),
            &map,
            |_, _| 0,
            members.iter().copied(),
        );
        assert_eq!(pick, Some(WithdrawSource::MemberHands { entity: alice }));
    }
}
