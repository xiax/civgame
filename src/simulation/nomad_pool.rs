//! Band-level inventory equalization for nomadic factions.
//!
//! Without a `FactionStorageTile`, nomads pool by carrying. If one member
//! ends up holding 3 bedrolls while four others have none — or one carries
//! the band's only packed yurt at 99% capacity and can't pick up their own
//! sleeping kit — the camp loses essentials on migration teardown. This
//! module redistributes "essential" resources across the band so capacity
//! is balanced and no one strands a critical item for lack of weight.
//!
//! Periodic system: `nomad_band_pool_balance_system` runs every
//! `POOL_BALANCE_INTERVAL` ticks (game-quarter-day) on Economy schedule for
//! factions whose `caps.storage` is `MemberPool` or `Hybrid`. The pure
//! allocator core is `redistribute_essentials`; it's separated so unit
//! tests exercise transfer logic without an `App`.
//!
//! Safety: the allocator removes from the donor only after measuring both
//! donor stock and recipient capacity, then rolls back on the donor if the
//! recipient's `add_resource` rejects any units (defensive — should never
//! fire when capacity was sized correctly, but `EconomicAgent::add_item`
//! returns "did not fit" silently and the consequence of trusting it is
//! item duplication).

use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::archetype::StorageBackendKind;
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::lod::LodLevel;
use crate::simulation::schedule::SimClock;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::TILE_SIZE;

/// Cadence for the periodic band-equalization sweep — quarter-day so a
/// daily migration trigger has at least one prior balance pass to draw on.
pub const POOL_BALANCE_INTERVAL: u64 = TICKS_PER_DAY as u64 / 4;

/// Members further than this from the band's `home_tile` (chebyshev) are
/// skipped — stragglers shouldn't get raided of their bedroll just because
/// the band is rebalancing.
pub const POOL_BAND_RADIUS: i32 = 12;

#[derive(Debug, Default, Clone, Copy)]
pub struct RedistributionReport {
    pub transfers: u32,
    pub units_moved: u32,
}

/// Pure allocator. Walks `essentials` and shrinks the max−min holding
/// spread per resource down to ≤ 1 unit. Works correctly whether the band
/// holds fewer units than members (target = 0, spread to 0/1 across
/// holders) or more (spread to floor / floor+1).
///
/// Algorithm per resource: while `max(q) − min(q) > 1` and a recipient
/// with capacity exists, transfer 1 unit from the highest holder to the
/// lowest. Bounded by `members² × essentials` iterations as a safety net.
///
/// Transfer is donor-remove-first then recipient-add, with rollback on
/// the donor if `add_resource` rejects (defensive against weight-cap edge
/// cases — `EconomicAgent::add_item` returns the unfit qty silently and
/// trusting it would silently duplicate).
pub fn redistribute_essentials(
    members: &mut [(Entity, &mut EconomicAgent)],
    essentials: &[ResourceId],
) -> RedistributionReport {
    let mut report = RedistributionReport::default();
    if members.len() < 2 {
        return report;
    }
    for &rid in essentials {
        let unit_w = rid.unit_weight_g().max(1);
        let band_total: u32 = members
            .iter()
            .map(|(_, a)| a.quantity_of_resource(rid))
            .sum();
        if band_total == 0 {
            continue;
        }

        // Bound the loop: in the worst case each transfer reduces the
        // max-by-1, and we'd never need more than band_total transfers.
        let cap_iter = (band_total as usize).saturating_mul(2).max(members.len());
        for _ in 0..cap_iter {
            // Find max holder and min holder with capacity.
            let mut max: Option<(usize, u32)> = None;
            let mut min: Option<(usize, u32)> = None;
            for (i, (_, a)) in members.iter().enumerate() {
                let q = a.quantity_of_resource(rid);
                if max.map_or(true, |(_, mq)| q > mq) {
                    max = Some((i, q));
                }
                let free_units = a.free_capacity_g() / unit_w;
                if free_units == 0 {
                    continue;
                }
                if min.map_or(true, |(_, mq)| q < mq) {
                    min = Some((i, q));
                }
            }
            let (Some((di, mq)), Some((ri, lq))) = (max, min) else {
                break;
            };
            // Stop when the spread is already ≤ 1 — moving 1 unit from a
            // holder to one with 1 less just swaps the imbalance.
            if di == ri || mq.saturating_sub(lq) <= 1 {
                break;
            }

            // Borrow split for disjoint &mut access.
            let (lo, hi) = if di < ri { (di, ri) } else { (ri, di) };
            let (left, right) = members.split_at_mut(hi);
            let (donor_agent, recip_agent) = if di < ri {
                (&mut *left[lo].1, &mut *right[0].1)
            } else {
                (&mut *right[0].1, &mut *left[lo].1)
            };

            // Transfer 1 unit per iteration — keeps the algo simple and
            // makes weight-cap edge cases trivially safe.
            let removed = donor_agent.remove_resource(rid, 1);
            if removed == 0 {
                break;
            }
            let unfit = recip_agent.add_resource(rid, removed);
            if unfit > 0 {
                // Roll back — recipient couldn't actually accept.
                let _ = donor_agent.add_resource(rid, unfit);
                break;
            }
            report.transfers = report.transfers.saturating_add(1);
            report.units_moved = report.units_moved.saturating_add(1);
        }
    }
    report
}

/// Periodic sweep — Economy schedule. For each nomadic faction, pulls live
/// band members within `POOL_BAND_RADIUS` of `home_tile` and runs the
/// allocator across them.
pub fn nomad_band_pool_balance_system(
    registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    mut q: Query<(
        Entity,
        &FactionMember,
        &mut EconomicAgent,
        &Transform,
        Option<&LodLevel>,
    )>,
) {
    if clock.tick % POOL_BALANCE_INTERVAL != 0 {
        return;
    }
    let essentials = essentials_for_band();
    if essentials.is_empty() {
        return;
    }

    // Group members by their faction's *root* (so household members of a
    // nomadic village pool with the band, not just within their household).
    let mut by_faction: ahash::AHashMap<u32, Vec<Entity>> = ahash::AHashMap::new();
    for (e, member, _agent, transform, lod) in q.iter() {
        if matches!(lod, Some(LodLevel::Dormant)) {
            continue;
        }
        let root = registry.root_faction(member.faction_id);
        let Some(faction) = registry.factions.get(&root) else {
            continue;
        };
        if !matches!(
            faction.caps.storage,
            StorageBackendKind::MemberPool | StorageBackendKind::Hybrid
        ) {
            continue;
        }
        let home = faction.home_tile;
        let tile = transform_tile(transform);
        if chebyshev(tile, home) > POOL_BAND_RADIUS {
            continue;
        }
        by_faction.entry(root).or_default().push(e);
    }

    // Snapshot-then-writeback pattern. `EconomicAgent: Copy`, so the
    // round-trip is cheap and avoids the `iter_many_mut` reborrow tangle.
    let mut updates: ahash::AHashMap<Entity, EconomicAgent> = ahash::AHashMap::new();
    for (_root, ents) in by_faction.iter() {
        if ents.len() < 2 {
            continue;
        }
        let mut snapshot: Vec<(Entity, EconomicAgent)> = ents
            .iter()
            .filter_map(|e| q.get(*e).ok().map(|(ent, _, a, _, _)| (ent, *a)))
            .collect();
        let mut view: Vec<(Entity, &mut EconomicAgent)> =
            snapshot.iter_mut().map(|(e, a)| (*e, &mut *a)).collect();
        let report = redistribute_essentials(&mut view, &essentials);
        if report.units_moved == 0 {
            continue;
        }
        for (e, a) in snapshot.into_iter() {
            updates.insert(e, a);
        }
    }
    // Writeback pass.
    for (e, _, mut agent, _, _) in q.iter_mut() {
        if let Some(updated) = updates.get(&e) {
            *agent = *updated;
        }
    }
}

/// Static list of resources the band guarantees a balanced share of.
pub fn essentials_for_band() -> Vec<ResourceId> {
    use crate::economy::core_ids;
    vec![
        core_ids::bedroll(),
        core_ids::packed_yurt(),
        core_ids::preserved_meat(),
    ]
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

#[inline]
fn transform_tile(transform: &Transform) -> (i32, i32) {
    let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
    let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
    (tx, ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::agent::{EconomicAgent, BASE_INVENTORY_CAP_G, INVENTORY_SLOTS};
    use crate::economy::item::Item;

    fn fresh_agent() -> EconomicAgent {
        // Redistribution tests are about pool-balance logic, not personal
        // capacity. Give the test agent oversized weight + volume budgets so
        // bedroll caps don't truncate the setup.
        EconomicAgent {
            currency: 0.0,
            inventory: [(Item::new_commodity(crate::economy::core_ids::fruit()), 0);
                INVENTORY_SLOTS],
            base_cap_g: BASE_INVENTORY_CAP_G * 10,
            bonus_cap_g: 0,
            base_small_vol_ml: u32::MAX / 2,
            base_bulky_vol_ml: u32::MAX / 2,
            bonus_small_vol_ml: 0,
            bonus_bulky_vol_ml: 0,
        }
    }

    fn install_test_catalog() {
        // Idempotent: `core_ids::catalog()` lazy-loads on first read and
        // primes every snake-case accessor's OnceLock as a side effect.
        let _ = crate::economy::core_ids::catalog();
    }

    #[test]
    fn redistribution_evens_essentials() {
        install_test_catalog();
        let bedroll = crate::economy::core_ids::bedroll();

        let mut agents: Vec<EconomicAgent> = (0..5).map(|_| fresh_agent()).collect();
        // One member starts with 3 bedrolls, the rest with none.
        agents[0].add_resource(bedroll, 3);
        {
            let entities: Vec<Entity> = (0..5).map(|i| Entity::from_raw(i + 1)).collect();
            let mut view: Vec<(Entity, &mut EconomicAgent)> = entities
                .iter()
                .copied()
                .zip(agents.iter_mut())
                .map(|(e, a)| (e, a))
                .collect();
            let report = redistribute_essentials(&mut view, &[bedroll]);
            assert!(report.units_moved >= 2, "report: {:?}", report);
        }

        // Total preserved across band.
        let total: u32 = agents.iter().map(|a| a.quantity_of_resource(bedroll)).sum();
        assert_eq!(total, 3, "no item should vanish or duplicate");
        // 3 bedrolls / 5 members → floor target = 0, so the donor's surplus
        // gets shed down toward 1 (we only move when q > target+1).
        let max_held = agents
            .iter()
            .map(|a| a.quantity_of_resource(bedroll))
            .max()
            .unwrap();
        assert!(
            max_held <= 2,
            "donor should not retain 3 bedrolls; max={max_held}"
        );
    }

    #[test]
    fn full_capacity_recipient_keeps_donor_intact() {
        install_test_catalog();
        let bedroll = crate::economy::core_ids::bedroll();
        let mut donor = fresh_agent();
        let mut recipient = fresh_agent();
        donor.add_resource(bedroll, 4);
        // Saturate recipient by zeroing both bulky-vol and weight caps so
        // no bedroll (OneHand, 12 L, 1.5 kg) can fit. Tests cap-rejection
        // semantics; the saturation mechanism is orthogonal.
        recipient.base_cap_g = 0;
        recipient.base_bulky_vol_ml = 0;
        let pre_donor = donor.quantity_of_resource(bedroll);
        let pre_total = pre_donor + recipient.quantity_of_resource(bedroll);

        let mut view: Vec<(Entity, &mut EconomicAgent)> = vec![
            (Entity::from_raw(1), &mut donor),
            (Entity::from_raw(2), &mut recipient),
        ];
        let _ = redistribute_essentials(&mut view, &[bedroll]);

        // No items lost, donor still has every bedroll (recipient had no room).
        let post_total =
            donor.quantity_of_resource(bedroll) + recipient.quantity_of_resource(bedroll);
        assert_eq!(post_total, pre_total, "no items vanish on cap rejection");
        assert_eq!(donor.quantity_of_resource(bedroll), pre_donor);
    }

    #[test]
    fn no_transfer_when_already_balanced() {
        install_test_catalog();
        let bedroll = crate::economy::core_ids::bedroll();
        let mut a = fresh_agent();
        let mut b = fresh_agent();
        a.add_resource(bedroll, 1);
        b.add_resource(bedroll, 1);
        let mut view: Vec<(Entity, &mut EconomicAgent)> =
            vec![(Entity::from_raw(1), &mut a), (Entity::from_raw(2), &mut b)];
        let report = redistribute_essentials(&mut view, &[bedroll]);
        assert_eq!(report.units_moved, 0);
    }
}
