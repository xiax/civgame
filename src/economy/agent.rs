use super::goods::Good;
use super::item::Item;
use bevy::prelude::*;

const INVENTORY_SLOTS: usize = 4;

/// Currency + small fixed inventory.
#[derive(Component, Clone, Copy)]
pub struct EconomicAgent {
    pub currency: f32,
    pub inventory: [(Item, u32); INVENTORY_SLOTS],
}

impl Default for EconomicAgent {
    fn default() -> Self {
        Self {
            currency: 50.0,
            inventory: [(Item::new_commodity(Good::Fruit), 0); INVENTORY_SLOTS],
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

    pub fn add_item(&mut self, item: Item, qty: u32) {
        // Find existing slot with identical item
        for (it, q) in self.inventory.iter_mut() {
            if *it == item && *q > 0 {
                *q = q.saturating_add(qty);
                return;
            }
        }
        // Find empty slot (qty == 0, treated as empty)
        for (it, q) in self.inventory.iter_mut() {
            if *q == 0 {
                *it = item;
                *q = qty;
                return;
            }
        }
    }

    pub fn add_good(&mut self, good: Good, qty: u32) {
        self.add_item(Item::new_commodity(good), qty);
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

    pub fn is_inventory_full(&self) -> bool {
        self.inventory.iter().all(|(_, q)| *q > 0)
    }
}
