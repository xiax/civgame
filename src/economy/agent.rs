use bevy::prelude::*;
use super::goods::Good;

const INVENTORY_SLOTS: usize = 4;

/// 24 bytes — currency + small fixed inventory.
#[derive(Component, Clone, Copy)]
pub struct EconomicAgent {
    pub currency:  f32,
    pub inventory: [(Good, u8); INVENTORY_SLOTS],
}

impl Default for EconomicAgent {
    fn default() -> Self {
        Self {
            currency: 50.0,
            inventory: [(Good::Food, 0); INVENTORY_SLOTS],
        }
    }
}

impl EconomicAgent {
    pub fn quantity_of(&self, good: Good) -> u8 {
        self.inventory.iter()
            .filter(|(g, _)| *g == good)
            .map(|(_, q)| *q)
            .sum()
    }

    pub fn add_good(&mut self, good: Good, qty: u8) {
        // Find existing slot
        for (g, q) in self.inventory.iter_mut() {
            if *g == good {
                *q = q.saturating_add(qty);
                return;
            }
        }
        // Find empty slot (qty == 0, treated as empty)
        for (g, q) in self.inventory.iter_mut() {
            if *q == 0 {
                *g = good;
                *q = qty;
                return;
            }
        }
        // Inventory full — drop the item (simplified)
    }

    /// Remove up to `qty` units of `good`. Returns how many were actually removed.
    pub fn remove_good(&mut self, good: Good, qty: u8) -> u8 {
        for (g, q) in self.inventory.iter_mut() {
            if *g == good && *q > 0 {
                let removed = (*q).min(qty);
                *q -= removed;
                return removed;
            }
        }
        0
    }

    pub fn has_tool(&self) -> bool {
        self.quantity_of(Good::Tools) > 0
    }

    pub fn is_inventory_full(&self) -> bool {
        self.inventory.iter().all(|(_, q)| *q > 0)
    }
}
