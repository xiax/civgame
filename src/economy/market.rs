use super::core_ids;
use super::item::Item;
use super::mode::EconomicMode;
use super::resource_catalog::ResourceId;
use ahash::AHashMap;
use bevy::prelude::*;

/// Walrasian market — prices adjust toward supply/demand equilibrium.
///
/// Phase 2 residual #2: the price/supply/demand/floor/ceiling/stock arrays
/// were `[f32; GOOD_COUNT]` indexed by `Good as usize`. They're now sparse
/// `AHashMap<ResourceId, f32>` keyed by catalog id — same shape used by
/// `FactionStorage.totals` (Phase 2d-ii). Lookups go through helper methods
/// that accept legacy `Good`; new code should index by `ResourceId` directly.
///
/// Pluralist Economy R1: `Market` is the **global SOLO/fallback** market;
/// settled factions get per-settlement `SettlementMarket`s (the same shape
/// minus the `Resource` impl) which take over in R7. The two share method
/// surface so call sites can swap the underlying instance with no logic
/// change.
#[derive(Resource)]
pub struct Market {
    prices: AHashMap<ResourceId, f32>,
    supply: AHashMap<ResourceId, f32>,
    demand: AHashMap<ResourceId, f32>,
    price_floor: AHashMap<ResourceId, f32>,
    price_ceiling: AHashMap<ResourceId, f32>,
    /// Physical commodities held in the market (Food, Wood, Iron, etc.)
    market_stock: AHashMap<ResourceId, f32>,
    /// Specific manufactured items available for purchase.
    pub listings: Vec<(Item, u32)>,
}

/// Per-settlement market state. Same fields and methods as `Market` but
/// not a `Resource` — lives on a Settlement entity. Activated in R7.
/// Defaults to fully empty (no seeded prices, no stock); a fresh
/// settlement's first sale establishes the initial price.
#[derive(Clone, Debug, Default)]
pub struct SettlementMarket {
    prices: AHashMap<ResourceId, f32>,
    supply: AHashMap<ResourceId, f32>,
    demand: AHashMap<ResourceId, f32>,
    price_floor: AHashMap<ResourceId, f32>,
    price_ceiling: AHashMap<ResourceId, f32>,
    market_stock: AHashMap<ResourceId, f32>,
    pub listings: Vec<(Item, u32)>,
}

impl SettlementMarket {
    fn price_id(&self, id: ResourceId) -> f32 {
        self.prices.get(&id).copied().unwrap_or(1.0)
    }

    fn floor_id(&self, id: ResourceId) -> f32 {
        self.price_floor.get(&id).copied().unwrap_or(DEFAULT_PRICE_FLOOR)
    }

    fn ceiling_id(&self, id: ResourceId) -> f32 {
        self.price_ceiling.get(&id).copied().unwrap_or(f32::INFINITY)
    }

    pub fn price_of(&self, id: ResourceId) -> f32 {
        self.price_id(id)
    }

    pub fn add_supply(&mut self, id: ResourceId, qty: f32) {
        *self.supply.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_demand(&mut self, id: ResourceId, qty: f32) {
        *self.demand.entry(id).or_insert(0.0) += qty;
    }

    pub fn stock_of(&self, id: ResourceId) -> f32 {
        self.market_stock.get(&id).copied().unwrap_or(0.0)
    }

    /// Tick prices. Mirrors `Market::update_prices` exactly so the two
    /// can swap places transparently in R7.
    pub fn update_prices(&mut self) {
        let mut active: Vec<ResourceId> = self.prices.keys().copied().collect();
        for id in self.supply.keys().chain(self.demand.keys()) {
            if !active.contains(id) {
                active.push(*id);
            }
        }
        for id in active {
            let supply = self.supply.get(&id).copied().unwrap_or(0.0);
            let demand = self.demand.get(&id).copied().unwrap_or(0.0);
            let ratio = (demand + 1.0) / (supply + 1.0);
            let cur = self.price_id(id);
            let next =
                (cur * ratio.powf(0.05)).clamp(self.floor_id(id), self.ceiling_id(id));
            self.prices.insert(id, next);
        }
    }
}

const DEFAULT_PRICE_FLOOR: f32 = 0.1;

impl Default for Market {
    fn default() -> Self {
        // Seed (resource_id_accessor, base_price, ceiling) tuples for the
        // 22 legacy goods; everything else defaults to floor=0.1,
        // ceiling=large, stock=0 on first trade.
        let seed_prices: [(fn() -> ResourceId, f32, f32); 22] = [
            (core_ids::fruit, 1.0, 50.0),
            (core_ids::meat, 1.2, 50.0),
            (core_ids::grain, 0.8, 50.0),
            (core_ids::wood, 0.8, 20.0),
            (core_ids::stone, 0.5, 10.0),
            (core_ids::tools, 2.0, 100.0),
            (core_ids::cloth, 1.5, 50.0),
            (core_ids::coal, 1.2, 30.0),
            (core_ids::iron, 1.8, 80.0),
            (core_ids::luxury, 5.0, 200.0),
            (core_ids::grain_seed, 0.5, 5.0),
            (core_ids::weapon, 3.0, 150.0),
            (core_ids::armor, 4.0, 180.0),
            (core_ids::shield, 2.5, 100.0),
            (core_ids::skin, 0.7, 20.0),
            (core_ids::copper, 2.0, 100.0),
            (core_ids::tin, 2.5, 120.0),
            (core_ids::gold, 25.0, 1000.0),
            (core_ids::silver, 10.0, 400.0),
            (core_ids::berry_seed, 0.4, 5.0),
            (core_ids::clay_tablet, 3.0, 80.0),
            (core_ids::book, 8.0, 200.0),
        ];

        let mut prices = AHashMap::new();
        let mut price_floor = AHashMap::new();
        let mut price_ceiling = AHashMap::new();
        let mut market_stock = AHashMap::new();

        for &(get_id, base_price, ceiling) in &seed_prices {
            let id = get_id();
            prices.insert(id, base_price);
            price_floor.insert(id, DEFAULT_PRICE_FLOOR);
            price_ceiling.insert(id, ceiling);
        }
        market_stock.insert(core_ids::tools(), 50.0);

        Self {
            prices,
            supply: AHashMap::new(),
            demand: AHashMap::new(),
            price_floor,
            price_ceiling,
            market_stock,
            listings: Vec::new(),
        }
    }
}

impl Market {
    fn price_id(&self, id: ResourceId) -> f32 {
        self.prices.get(&id).copied().unwrap_or(1.0)
    }

    fn floor_id(&self, id: ResourceId) -> f32 {
        self.price_floor.get(&id).copied().unwrap_or(DEFAULT_PRICE_FLOOR)
    }

    fn ceiling_id(&self, id: ResourceId) -> f32 {
        self.price_ceiling.get(&id).copied().unwrap_or(f32::INFINITY)
    }

    /// Current price for the resource identified by `id`. Returns 1.0 if
    /// no entry exists yet.
    pub fn price_of(&self, id: ResourceId) -> f32 {
        self.price_id(id)
    }

    pub fn add_supply(&mut self, id: ResourceId, qty: f32) {
        *self.supply.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_demand(&mut self, id: ResourceId, qty: f32) {
        *self.demand.entry(id).or_insert(0.0) += qty;
    }

    pub fn stock_of(&self, id: ResourceId) -> f32 {
        self.market_stock.get(&id).copied().unwrap_or(0.0)
    }

    pub fn update_prices(&mut self) {
        // Sweep every resource that has any market activity (price, supply,
        // demand, floor, ceiling, or stock entry). Sparse representation
        // means we never touch resources that have never traded.
        let mut active: Vec<ResourceId> = self.prices.keys().copied().collect();
        for id in self.supply.keys().chain(self.demand.keys()) {
            if !active.contains(id) {
                active.push(*id);
            }
        }
        for id in active {
            let supply = self.supply.get(&id).copied().unwrap_or(0.0);
            let demand = self.demand.get(&id).copied().unwrap_or(0.0);
            let ratio = (demand + 1.0) / (supply + 1.0);
            let cur = self.price_id(id);
            let next = (cur * ratio.powf(0.05)).clamp(self.floor_id(id), self.ceiling_id(id));
            self.prices.insert(id, next);
        }
    }

    pub fn calculate_price(&self, item: &Item) -> f32 {
        self.price_id(item.resource_id) * item.multiplier()
    }

    /// Agent sells an item to the market. Returns currency earned.
    pub fn sell_item(&mut self, item: Item, qty: u32) -> f32 {
        let price = self.calculate_price(&item);
        let total_earned = price * qty as f32;
        let id = item.resource_id;

        if item.is_manufactured() {
            // Add to specific listings
            if let Some(entry) = self.listings.iter_mut().find(|(it, _)| *it == item) {
                entry.1 += qty;
            } else {
                self.listings.push((item, qty));
            }
        } else {
            // Add to commodity pool
            *self.market_stock.entry(id).or_insert(0.0) += qty as f32;
        }

        *self.supply.entry(id).or_insert(0.0) += qty as f32;
        total_earned
    }

    /// Agent attempts to buy a specific `good` type.
    /// If manufactured, searches listings for affordable options.
    /// If commodity, buys from stock.
    pub fn try_buy_item(
        &mut self,
        id: ResourceId,
        qty: u32,
        currency: &mut f32,
    ) -> (Option<Item>, u32) {

        // 1. Check specific listings first if it's potentially manufactured
        // In this simple version, we'll try to buy the BEST quality item affordable.
        let mut best_idx: Option<usize> = None;
        let mut best_mult = -1.0;

        for (idx, (item, stock)) in self.listings.iter().enumerate() {
            if item.resource_id == id && *stock > 0 {
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
                *self.demand.entry(id).or_insert(0.0) += buy_qty as f32;
                return (Some(bought_item), buy_qty);
            }
        }

        // 2. Fallback to generic commodity stock
        let stock = self.market_stock.get(&id).copied().unwrap_or(0.0);
        let available = stock.min(qty as f32);
        if available <= 0.0 {
            return (None, 0);
        }

        let item = Item::new_commodity(id);
        let price = self.calculate_price(&item);
        let buy_qty = (available.floor() as u32).min(qty);
        let total_cost = price * buy_qty as f32;

        if total_cost > *currency || buy_qty == 0 {
            return (None, 0);
        }

        *currency -= total_cost;
        self.market_stock.insert(id, stock - buy_qty as f32);
        *self.demand.entry(id).or_insert(0.0) += buy_qty as f32;
        (Some(item), buy_qty)
    }

}

pub fn price_update_system(mut market: ResMut<Market>, mode: Res<EconomicMode>) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    // Background Food demand to prevent price collapse when all agents are fed
    market.add_demand(core_ids::fruit(), 2.0);
    market.add_demand(core_ids::meat(), 1.0);
    market.add_demand(core_ids::grain(), 2.0);
    market.update_prices();
    market.supply.clear();
    market.demand.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_rises_when_demand_exceeds_supply() {
        let mut m = Market::default();
        let fruit = core_ids::fruit();
        m.add_supply(fruit, 10.0);
        m.add_demand(fruit, 100.0);
        let old_price = m.price_of(fruit);
        m.update_prices();
        assert!(m.price_of(fruit) > old_price);
    }
}
