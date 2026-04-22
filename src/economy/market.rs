use bevy::prelude::*;
use super::goods::{Good, GOOD_COUNT};
use super::mode::EconomicMode;

/// Walrasian market — prices adjust toward supply/demand equilibrium.
#[derive(Resource)]
pub struct Market {
    pub prices:        [f32; GOOD_COUNT],
    pub supply:        [f32; GOOD_COUNT],
    pub demand:        [f32; GOOD_COUNT],
    pub price_floor:   [f32; GOOD_COUNT],
    pub price_ceiling: [f32; GOOD_COUNT],
    /// Physical goods held in the market available for purchase.
    pub market_stock:  [f32; GOOD_COUNT],
}

impl Default for Market {
    fn default() -> Self {
        let mut market_stock = [0.0f32; GOOD_COUNT];
        market_stock[Good::Tools as usize] = 500.0;
        Self {
            prices:        [1.0, 0.8, 0.5, 2.0, 1.5, 1.2, 1.8, 5.0, 0.5],
            supply:        [0.0; GOOD_COUNT],
            demand:        [0.0; GOOD_COUNT],
            price_floor:   [0.1; GOOD_COUNT],
            price_ceiling: [50.0, 20.0, 10.0, 100.0, 50.0, 30.0, 80.0, 200.0, 5.0],
            market_stock,
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

    /// Agent sells `qty` units to the market. Returns currency earned.
    pub fn sell(&mut self, good: Good, qty: f32) -> f32 {
        let i = good as usize;
        let earned = qty * self.prices[i];
        self.market_stock[i] += qty;
        self.supply[i] += qty;
        earned
    }

    /// Agent attempts to buy `qty` units. Returns units actually purchased.
    pub fn try_buy(&mut self, good: Good, qty: f32, currency: &mut f32) -> f32 {
        let i = good as usize;
        let available = self.market_stock[i].min(qty);
        if available <= 0.0 {
            return 0.0;
        }
        let cost = available * self.prices[i];
        if cost > *currency {
            return 0.0;
        }
        *currency -= cost;
        self.market_stock[i] -= available;
        self.demand[i] += available;
        available
    }
}

pub fn price_update_system(
    mut market: ResMut<Market>,
    mode: Res<EconomicMode>,
) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    // Background Food demand to prevent price collapse when all agents are fed
    market.demand[Good::Food as usize] += 5.0;
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
