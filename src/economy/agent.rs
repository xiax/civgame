use super::goods::Bulk;
use super::item::Item;
use super::resource_catalog::ResourceId;
use bevy::prelude::*;

/// Number of inventory stacks. Personal inventory is weight-capped; this is just an upper
/// bound on distinct stack types.
pub const INVENTORY_SLOTS: usize = 6;

/// Default base weight capacity for a person, in grams (~5 kg).
pub const BASE_INVENTORY_CAP_G: u32 = 5_000;

/// Default base small-bulk volume capacity for a person, in millilitres (~8 L).
/// Bounds `Bulk::Small` stacks (pouches / satchel).
pub const BASE_INVENTORY_SMALL_VOL_ML: u32 = 8_000;

/// Default base bulky volume capacity for a person, in millilitres (~25 L).
/// Bounds `Bulk::OneHand` / `Bulk::TwoHand` stacks (lashed pack / sack).
pub const BASE_INVENTORY_BULKY_VOL_ML: u32 = 25_000;

/// Currency + small fixed inventory.
#[derive(Component, Clone, Copy)]
pub struct EconomicAgent {
    pub currency: f32,
    pub inventory: [(Item, u32); INVENTORY_SLOTS],
    /// Base personal carrying capacity in grams.
    pub base_cap_g: u32,
    /// Bonus capacity in grams (recomputed from worn equipment).
    pub bonus_cap_g: u32,
    /// Base small-bulk volume capacity in millilitres.
    pub base_small_vol_ml: u32,
    /// Base bulky (OneHand/TwoHand) volume capacity in millilitres.
    pub base_bulky_vol_ml: u32,
    /// Bonus small-bulk volume in millilitres (future equipment lift).
    pub bonus_small_vol_ml: u32,
    /// Bonus bulky volume in millilitres (future equipment lift).
    pub bonus_bulky_vol_ml: u32,
}

impl Default for EconomicAgent {
    fn default() -> Self {
        Self {
            currency: 50.0,
            // Empty placeholder slots use Fruit's id; qty=0 means the slot
            // is unused (the underlying resource doesn't matter until a
            // real Item is stamped in).
            inventory: [(Item::new_commodity(crate::economy::core_ids::fruit()), 0);
                INVENTORY_SLOTS],
            base_cap_g: BASE_INVENTORY_CAP_G,
            bonus_cap_g: 0,
            base_small_vol_ml: BASE_INVENTORY_SMALL_VOL_ML,
            base_bulky_vol_ml: BASE_INVENTORY_BULKY_VOL_ML,
            bonus_small_vol_ml: 0,
            bonus_bulky_vol_ml: 0,
        }
    }
}

impl EconomicAgent {
    pub fn total_food(&self) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, _)| it.resource_id.is_edible())
            .fold(0u32, |acc, (_, q)| acc.saturating_add(*q))
    }

    /// Total weight capacity, in grams.
    pub fn capacity_g(&self) -> u32 {
        self.base_cap_g.saturating_add(self.bonus_cap_g)
    }

    /// Total small-bulk volume capacity, in millilitres.
    pub fn capacity_small_vol_ml(&self) -> u32 {
        self.base_small_vol_ml
            .saturating_add(self.bonus_small_vol_ml)
    }

    /// Total bulky (OneHand/TwoHand) volume capacity, in millilitres.
    pub fn capacity_bulky_vol_ml(&self) -> u32 {
        self.base_bulky_vol_ml
            .saturating_add(self.bonus_bulky_vol_ml)
    }

    /// Current inventory weight, in grams.
    pub fn current_weight_g(&self) -> u32 {
        self.inventory
            .iter()
            .filter(|(_, q)| *q > 0)
            .fold(0u32, |acc, (it, q)| {
                acc.saturating_add(it.stack_weight_g(*q))
            })
    }

    /// Current small-bulk volume, summed over `Bulk::Small` stacks.
    pub fn current_small_vol_ml(&self) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, q)| *q > 0 && matches!(it.resource_id.bulk(), Bulk::Small))
            .fold(0u32, |acc, (it, q)| {
                acc.saturating_add(it.stack_volume_ml(*q))
            })
    }

    /// Current bulky volume, summed over `Bulk::OneHand` / `Bulk::TwoHand` stacks.
    pub fn current_bulky_vol_ml(&self) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, q)| {
                *q > 0 && !matches!(it.resource_id.bulk(), Bulk::Small)
            })
            .fold(0u32, |acc, (it, q)| {
                acc.saturating_add(it.stack_volume_ml(*q))
            })
    }

    /// Remaining weight capacity, in grams.
    pub fn free_capacity_g(&self) -> u32 {
        self.capacity_g().saturating_sub(self.current_weight_g())
    }

    /// Remaining volume capacity for the bucket matching `item`'s bulk class.
    pub fn free_vol_ml_for(&self, item: Item) -> u32 {
        match item.resource_id.bulk() {
            Bulk::Small => self
                .capacity_small_vol_ml()
                .saturating_sub(self.current_small_vol_ml()),
            Bulk::OneHand | Bulk::TwoHand => self
                .capacity_bulky_vol_ml()
                .saturating_sub(self.current_bulky_vol_ml()),
        }
    }

    /// Try to add `qty` of `item`. Returns the amount that did not fit (0 if all fit).
    /// Constraints: weight cap, per-bucket volume cap, and slot count.
    pub fn add_item(&mut self, item: Item, qty: u32) -> u32 {
        if qty == 0 {
            return 0;
        }
        let unit_w = item.unit_weight_g().max(1);
        let unit_v = item.unit_volume_ml().max(1);
        let cap_w = self.capacity_g();
        let bulk = item.resource_id.bulk();
        let (cap_v, mut used_v) = match bulk {
            Bulk::Small => (self.capacity_small_vol_ml(), self.current_small_vol_ml()),
            Bulk::OneHand | Bulk::TwoHand => (
                self.capacity_bulky_vol_ml(),
                self.current_bulky_vol_ml(),
            ),
        };
        let mut used_w = self.current_weight_g();
        let mut remaining = qty;

        // Top up an existing matching stack.
        for (it, q) in self.inventory.iter_mut() {
            if *q > 0 && *it == item {
                let weight_room = cap_w.saturating_sub(used_w);
                let vol_room = cap_v.saturating_sub(used_v);
                let by_weight = weight_room / unit_w;
                let by_volume = vol_room / unit_v;
                let take = remaining.min(by_weight).min(by_volume);
                if take == 0 {
                    return remaining;
                }
                *q = q.saturating_add(take);
                used_w = used_w.saturating_add(take.saturating_mul(unit_w));
                used_v = used_v.saturating_add(take.saturating_mul(unit_v));
                remaining -= take;
                if remaining == 0 {
                    return 0;
                }
                break;
            }
        }

        // Claim an empty slot.
        if remaining > 0 {
            let weight_room = cap_w.saturating_sub(used_w);
            let vol_room = cap_v.saturating_sub(used_v);
            let by_weight = weight_room / unit_w;
            let by_volume = vol_room / unit_v;
            let take = remaining.min(by_weight).min(by_volume);
            if take == 0 {
                return remaining;
            }
            for (it, q) in self.inventory.iter_mut() {
                if *q == 0 {
                    *it = item;
                    *q = take;
                    remaining -= take;
                    break;
                }
            }
        }

        remaining
    }

    /// Remove up to `qty` units of a specific `item`. Returns how many were actually removed.
    pub fn remove_item(&mut self, item: Item, qty: u32) -> u32 {
        for (it, q) in self.inventory.iter_mut() {
            if *it == item && *q > 0 {
                let removed = (*q).min(qty);
                *q -= removed;
                return removed;
            }
        }
        0
    }

    pub fn has_tool(&self) -> bool {
        self.quantity_of_resource(crate::economy::core_ids::tools()) > 0
    }

    // ── ResourceId-keyed inventory accessors ──
    //
    // These are the canonical implementations. The legacy `*_good` methods
    // are thin wrappers that convert via `core_ids::good_to_resource_id`;
    // they disappear once the `Good` enum does.

    /// Sum of `qty` across every inventory stack whose underlying
    /// resource matches `id`. Manufactured items collapse onto their base
    /// resource (e.g. a manufactured weapon counts as the base material).
    pub fn quantity_of_resource(&self, id: ResourceId) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, _)| it.resource_id == id)
            .fold(0u32, |acc, (_, q)| acc.saturating_add(*q))
    }

    /// Try to add `qty` of the resource identified by `id`. Returns the
    /// amount that did not fit.
    pub fn add_resource(&mut self, id: ResourceId, qty: u32) -> u32 {
        self.add_item(Item::new_commodity(id), qty)
    }

    /// Remove up to `qty` units of the resource identified by `id`.
    /// Returns how many were removed.
    pub fn remove_resource(&mut self, id: ResourceId, qty: u32) -> u32 {
        self.remove_item(Item::new_commodity(id), qty)
    }

    /// Iterate `(ResourceId, qty)` over every non-empty stack. The
    /// ResourceId is computed via `Item::resource_id()` so manufactured
    /// items collapse onto their base resource — caller can re-query the
    /// catalog for class/tag inspection without touching the legacy
    /// `Good` enum.
    pub fn iter_resource_stacks(&self) -> impl Iterator<Item = (ResourceId, u32)> + '_ {
        self.inventory
            .iter()
            .filter(|(_, q)| *q > 0)
            .map(|(it, q)| (it.resource_id, *q))
    }

    /// Inventory is full when no smallest-stack unit (by weight AND its
    /// matching bulk-bucket volume) fits, accounting for slot availability.
    pub fn is_inventory_full(&self) -> bool {
        let cap_w = self.capacity_g();
        let used_w = self.current_weight_g();
        let weight_room = cap_w.saturating_sub(used_w);
        let small_room = self
            .capacity_small_vol_ml()
            .saturating_sub(self.current_small_vol_ml());
        let bulky_room = self
            .capacity_bulky_vol_ml()
            .saturating_sub(self.current_bulky_vol_ml());
        if weight_room == 0 || (small_room == 0 && bulky_room == 0) {
            return true;
        }
        let any_empty = self.inventory.iter().any(|(_, q)| *q == 0);
        // Fits if any occupied stack can grow OR an empty slot can accept
        // a fresh stack of any held item type within both budgets.
        let any_fits = self.inventory.iter().any(|(it, q)| {
            if *q == 0 {
                return false;
            }
            let unit_w = it.unit_weight_g().max(1);
            let unit_v = it.unit_volume_ml().max(1);
            let vol_room = match it.resource_id.bulk() {
                Bulk::Small => small_room,
                Bulk::OneHand | Bulk::TwoHand => bulky_room,
            };
            unit_w <= weight_room && unit_v <= vol_room
        });
        // An empty slot with positive weight+volume room makes us not full
        // (conservative — caller adding determines exact qty).
        !(any_fits || any_empty)
    }
}
