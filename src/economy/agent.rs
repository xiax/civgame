use super::goods::Good;
use super::item::Item;
use bevy::prelude::*;

/// Number of inventory stacks. Personal inventory is weight-capped; this is just an upper
/// bound on distinct stack types.
pub const INVENTORY_SLOTS: usize = 6;

/// Default base weight capacity for a person, in grams (~5 kg).
pub const BASE_INVENTORY_CAP_G: u32 = 5_000;

/// Currency + small fixed inventory.
#[derive(Component, Clone, Copy)]
pub struct EconomicAgent {
    pub currency: f32,
    pub inventory: [(Item, u32); INVENTORY_SLOTS],
    /// Base personal carrying capacity in grams.
    pub base_cap_g: u32,
    /// Bonus capacity in grams (recomputed from worn equipment).
    pub bonus_cap_g: u32,
}

impl Default for EconomicAgent {
    fn default() -> Self {
        Self {
            currency: 50.0,
            inventory: [(Item::new_commodity(Good::Fruit), 0); INVENTORY_SLOTS],
            base_cap_g: BASE_INVENTORY_CAP_G,
            bonus_cap_g: 0,
        }
    }
}

impl EconomicAgent {
    pub fn total_food(&self) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, _)| it.good.is_edible())
            .fold(0u32, |acc, (_, q)| acc.saturating_add(*q))
    }

    pub fn quantity_of(&self, good: Good) -> u32 {
        self.inventory
            .iter()
            .filter(|(it, _)| it.good == good)
            .fold(0u32, |acc, (_, q)| acc.saturating_add(*q))
    }

    /// Total weight capacity, in grams.
    pub fn capacity_g(&self) -> u32 {
        self.base_cap_g.saturating_add(self.bonus_cap_g)
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

    /// Remaining weight capacity, in grams.
    pub fn free_capacity_g(&self) -> u32 {
        self.capacity_g().saturating_sub(self.current_weight_g())
    }

    /// Try to add `qty` of `item`. Returns the amount that did not fit (0 if all fit).
    /// Constraints: weight cap and slot count.
    pub fn add_item(&mut self, item: Item, qty: u32) -> u32 {
        if qty == 0 {
            return 0;
        }
        let unit_w = item.unit_weight_g().max(1);
        let cap = self.capacity_g();
        let mut used = self.current_weight_g();
        let mut remaining = qty;

        // Top up an existing matching stack.
        for (it, q) in self.inventory.iter_mut() {
            if *q > 0 && *it == item {
                let cap_left = cap.saturating_sub(used);
                let by_weight = cap_left / unit_w;
                if by_weight == 0 {
                    return remaining;
                }
                let take = remaining.min(by_weight);
                *q = q.saturating_add(take);
                used = used.saturating_add(take.saturating_mul(unit_w));
                remaining -= take;
                if remaining == 0 {
                    return 0;
                }
                break;
            }
        }

        // Claim an empty slot.
        if remaining > 0 {
            let cap_left = cap.saturating_sub(used);
            let by_weight = cap_left / unit_w;
            if by_weight == 0 {
                return remaining;
            }
            let take = remaining.min(by_weight);
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

    pub fn add_good(&mut self, good: Good, qty: u32) -> u32 {
        self.add_item(Item::new_commodity(good), qty)
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

    pub fn remove_good(&mut self, good: Good, qty: u32) -> u32 {
        self.remove_item(Item::new_commodity(good), qty)
    }

    pub fn has_tool(&self) -> bool {
        self.quantity_of(Good::Tools) > 0
    }

    /// Inventory is full when no more weight can fit (using the smallest stocked item as a
    /// rough lower bound, or any item if empty).
    pub fn is_inventory_full(&self) -> bool {
        let cap = self.capacity_g();
        let used = self.current_weight_g();
        if used >= cap {
            return true;
        }
        // Slots all in use AND no existing stack can grow within the weight budget?
        let any_empty = self.inventory.iter().any(|(_, q)| *q == 0);
        if any_empty {
            return false;
        }
        // All slots occupied: inventory is "full" if no smallest-stack unit fits.
        let cap_left = cap - used;
        !self
            .inventory
            .iter()
            .any(|(it, q)| *q > 0 && it.unit_weight_g() <= cap_left)
    }
}
