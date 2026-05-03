use super::goods::{Good, GOOD_COUNT};
use super::item::Item;
use super::mode::EconomicMode;
use bevy::prelude::*;

/// Walrasian market — prices adjust toward supply/demand equilibrium.
#[derive(Resource)]
pub struct Market {
    pub prices: [f32; GOOD_COUNT],
    pub supply: [f32; GOOD_COUNT],
    pub demand: [f32; GOOD_COUNT],
    pub price_floor: [f32; GOOD_COUNT],
    pub price_ceiling: [f32; GOOD_COUNT],
    /// Physical commodities held in the market (Food, Wood, Iron, etc.)
    pub market_stock: [f32; GOOD_COUNT],
    /// Specific manufactured items available for purchase.
    pub listings: Vec<(Item, u32)>,
}

impl Default for Market {
    fn default() -> Self {
        let mut market_stock = [0.0f32; GOOD_COUNT];
        market_stock[Good::Tools as usize] = 50.0; // Startup supply of generic tools
        Self {
            prices: [
                1.0, 1.2, 0.8, 0.8, 0.5, 2.0, 1.5, 1.2, 1.8, 5.0, 0.5, 3.0, 4.0, 2.5, 0.7, 2.0,
                2.5, 25.0, 10.0, 0.4, // BerrySeed
            ],
            supply: [0.0; GOOD_COUNT],
            demand: [0.0; GOOD_COUNT],
            price_floor: [0.1; GOOD_COUNT],
            price_ceiling: [
                50.0, 50.0, 50.0, 20.0, 10.0, 100.0, 50.0, 30.0, 80.0, 200.0, 5.0, 150.0, 180.0,
                100.0, 20.0, 100.0, 120.0, 1000.0, 400.0, 5.0, // BerrySeed
            ],
            market_stock,
            listings: Vec::new(),
        }
    }
}

impl Market {
    pub fn update_prices(&mut self) {
        for i in 0..GOOD_COUNT {
            let ratio = (self.demand[i] + 1.0) / (self.supply[i] + 1.0);
            self.prices[i] = (self.prices[i] * ratio.powf(0.05))
                .clamp(self.price_floor[i], self.price_ceiling[i]);
        }
    }

    pub fn calculate_price(&self, item: &Item) -> f32 {
        let base_price = self.prices[item.good as usize];
        base_price * item.multiplier()
    }

    /// Agent sells an item to the market. Returns currency earned.
    pub fn sell_item(&mut self, item: Item, qty: u32) -> f32 {
        let price = self.calculate_price(&item);
        let total_earned = price * qty as f32;

        if item.is_manufactured() {
            // Add to specific listings
            if let Some(entry) = self.listings.iter_mut().find(|(it, _)| *it == item) {
                entry.1 += qty;
            } else {
                self.listings.push((item, qty));
            }
        } else {
            // Add to commodity pool
            self.market_stock[item.good as usize] += qty as f32;
        }

        self.supply[item.good as usize] += qty as f32;
        total_earned
    }

    /// Agent attempts to buy a specific `good` type.
    /// If manufactured, searches listings for affordable options.
    /// If commodity, buys from stock.
    pub fn try_buy_item(
        &mut self,
        good: Good,
        qty: u32,
        currency: &mut f32,
    ) -> (Option<Item>, u32) {
        let i = good as usize;

        // 1. Check specific listings first if it's potentially manufactured
        // In this simple version, we'll try to buy the BEST quality item affordable.
        let mut best_idx: Option<usize> = None;
        let mut best_mult = -1.0;

        for (idx, (item, stock)) in self.listings.iter().enumerate() {
            if item.good == good && *stock > 0 {
                let price = self.calculate_price(item);
                if price <= *currency {
                    let mult = item.multiplier();
                    if mult > best_mult {
                        best_mult = mult;
                        best_idx = Some(idx);
                    }
                }
            }
        }

        if let Some(idx) = best_idx {
            let item = self.listings[idx].0;
            let price = self.calculate_price(&item);
            let buy_qty = qty.min(self.listings[idx].1);
            let total_cost = price * buy_qty as f32;

            if total_cost <= *currency {
                *currency -= total_cost;
                self.listings[idx].1 -= buy_qty;
                let bought_item = item;
                self.demand[i] += buy_qty as f32;
                return (Some(bought_item), buy_qty);
            }
        }

        // 2. Fallback to generic commodity stock
        let available = self.market_stock[i].min(qty as f32);
        if available <= 0.0 {
            return (None, 0);
        }

        let item = Item::new_commodity(good);
        let price = self.calculate_price(&item);
        let buy_qty = (available.floor() as u32).min(qty);
        let total_cost = price * buy_qty as f32;

        if total_cost > *currency || buy_qty == 0 {
            return (None, 0);
        }

        *currency -= total_cost;
        self.market_stock[i] -= buy_qty as f32;
        self.demand[i] += buy_qty as f32;
        (Some(item), buy_qty)
    }

    /// Legacy support for simple Good selling
    pub fn sell(&mut self, good: Good, qty: f32) -> f32 {
        self.sell_item(Item::new_commodity(good), qty as u32)
    }

    /// Legacy support for simple Good buying
    pub fn try_buy(&mut self, good: Good, qty: f32, currency: &mut f32) -> f32 {
        let (item, bought) = self.try_buy_item(good, qty as u32, currency);
        if item.is_some() {
            bought as f32
        } else {
            0.0
        }
    }
}

pub fn price_update_system(mut market: ResMut<Market>, mode: Res<EconomicMode>) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    // Background Food demand to prevent price collapse when all agents are fed
    market.demand[Good::Fruit as usize] += 2.0;
    market.demand[Good::Meat as usize] += 1.0;
    market.demand[Good::Grain as usize] += 2.0;
    market.update_prices();
    market.supply = [0.0; GOOD_COUNT];
    market.demand = [0.0; GOOD_COUNT];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_rises_when_demand_exceeds_supply() {
        let mut m = Market::default();
        m.supply[0] = 10.0;
        m.demand[0] = 100.0;
        let old_price = m.prices[0];
        m.update_prices();
        assert!(m.prices[0] > old_price);
    }
}
