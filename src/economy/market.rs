use super::core_ids;
use super::item::Item;
use super::mode::EconomicMode;
use super::resource_catalog::ResourceId;
use ahash::AHashMap;
use bevy::prelude::*;

/// Bid-driven market — prices reflect what buyers are willing and able
/// to pay. Each tick the market aggregates buyer-attempt outcomes:
/// - `bids_cleared`: successful purchase at posted price (no signal)
/// - `bids_stockout`: would-buy attempt blocked by empty stock (push UP)
/// - `bids_unaffordable`: would-buy attempt blocked by buyer's currency (push DOWN)
///
/// `signal = (stockout - unaffordable) / total_activity` is bounded in
/// `[-1, 1]`; price drifts by up to ±5% per tick. Quiet markets (no buyer
/// activity) leave price unchanged. The natural ceiling is buyer wallets
/// — runaway price empties `cleared` and floods `unaffordable`, flipping
/// the sign. The natural floor is a small `DEFAULT_PRICE_FLOOR` to avoid
/// numerical pathology around zero.
///
/// Pluralist Economy R1: `Market` is the **global SOLO/fallback** market;
/// settled factions get per-settlement `SettlementMarket`s (the same shape
/// minus the `Resource` impl) which take over in R7.
#[derive(Resource)]
pub struct Market {
    prices: AHashMap<ResourceId, f32>,
    bids_cleared: AHashMap<ResourceId, f32>,
    bids_stockout: AHashMap<ResourceId, f32>,
    bids_unaffordable: AHashMap<ResourceId, f32>,
    price_floor: AHashMap<ResourceId, f32>,
    /// Physical commodities held in the market (Food, Wood, Iron, etc.)
    market_stock: AHashMap<ResourceId, f32>,
    /// Specific manufactured items available for purchase.
    pub listings: Vec<(Item, u32)>,
}

/// Per-settlement market state. Same fields and methods as `Market` but
/// not a `Resource` — lives on a Settlement entity. Activated in R7.
/// Defaults to fully empty: implicit price `1.0` for any resource until
/// real buyer activity occurs.
#[derive(Clone, Debug, Default)]
pub struct SettlementMarket {
    prices: AHashMap<ResourceId, f32>,
    bids_cleared: AHashMap<ResourceId, f32>,
    bids_stockout: AHashMap<ResourceId, f32>,
    bids_unaffordable: AHashMap<ResourceId, f32>,
    price_floor: AHashMap<ResourceId, f32>,
    market_stock: AHashMap<ResourceId, f32>,
    pub listings: Vec<(Item, u32)>,
}

const DEFAULT_PRICE_FLOOR: f32 = 0.1;
const PRICE_DRIFT_PER_TICK: f32 = 0.05;

impl SettlementMarket {
    fn price_id(&self, id: ResourceId) -> f32 {
        self.prices.get(&id).copied().unwrap_or(1.0)
    }

    fn floor_id(&self, id: ResourceId) -> f32 {
        self.price_floor.get(&id).copied().unwrap_or(DEFAULT_PRICE_FLOOR)
    }

    pub fn price_of(&self, id: ResourceId) -> f32 {
        self.price_id(id)
    }

    pub fn add_bid_cleared(&mut self, id: ResourceId, qty: f32) {
        *self.bids_cleared.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_bid_stockout(&mut self, id: ResourceId, qty: f32) {
        *self.bids_stockout.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_bid_unaffordable(&mut self, id: ResourceId, qty: f32) {
        *self.bids_unaffordable.entry(id).or_insert(0.0) += qty;
    }

    pub fn stock_of(&self, id: ResourceId) -> f32 {
        self.market_stock.get(&id).copied().unwrap_or(0.0)
    }

    /// Direct stock setter. Used by trader buy/sell helpers and test
    /// fixtures so they can update commodity stock without round-tripping
    /// through `try_buy_item` / `sell_item`.
    pub fn set_stock(&mut self, id: ResourceId, qty: f32) {
        if qty <= 0.0 {
            self.market_stock.remove(&id);
        } else {
            self.market_stock.insert(id, qty);
        }
    }

    /// Test/admin override: force-set the posted price for `id`. Bypasses
    /// the bid-driven discovery process. Subsequent buyer activity will
    /// nudge the price from this value via the normal signal.
    pub fn set_price(&mut self, id: ResourceId, price: f32) {
        let floor = self.floor_id(id);
        self.prices.insert(id, price.max(floor));
    }

    /// Bid-driven price update. See struct doc.
    pub fn update_prices(&mut self) {
        let mut active: Vec<ResourceId> = self.prices.keys().copied().collect();
        for id in self
            .bids_cleared
            .keys()
            .chain(self.bids_stockout.keys())
            .chain(self.bids_unaffordable.keys())
        {
            if !active.contains(id) {
                active.push(*id);
            }
        }
        for id in active {
            let cleared = self.bids_cleared.get(&id).copied().unwrap_or(0.0);
            let stockout = self.bids_stockout.get(&id).copied().unwrap_or(0.0);
            let unaffordable = self.bids_unaffordable.get(&id).copied().unwrap_or(0.0);
            let activity = cleared + stockout + unaffordable;
            if activity < 1.0 {
                continue;
            }
            let signal = (stockout - unaffordable) / activity;
            let cur = self.price_id(id);
            let next = (cur * (1.0 + signal * PRICE_DRIFT_PER_TICK)).max(self.floor_id(id));
            self.prices.insert(id, next);
        }
    }

    /// Reset per-tick bid counters. Called by `settlement_price_update_system`
    /// after `update_prices`.
    pub fn clear_flow(&mut self) {
        self.bids_cleared.clear();
        self.bids_stockout.clear();
        self.bids_unaffordable.clear();
    }

    pub fn calculate_price(&self, item: &Item) -> f32 {
        self.price_id(item.resource_id) * item.multiplier()
    }

    /// Agent sells an item. Returns currency earned. Sale always succeeds
    /// (sellers don't crash the price — sale flow is not a price signal).
    pub fn sell_item(&mut self, item: Item, qty: u32) -> f32 {
        let price = self.calculate_price(&item);
        let total_earned = price * qty as f32;
        let id = item.resource_id;
        if item.is_manufactured() {
            if let Some(entry) = self.listings.iter_mut().find(|(it, _)| *it == item) {
                entry.1 += qty;
            } else {
                self.listings.push((item, qty));
            }
        } else {
            *self.market_stock.entry(id).or_insert(0.0) += qty as f32;
        }
        total_earned
    }

    /// Agent attempts to buy. Records the outcome as a bid signal.
    pub fn try_buy_item(
        &mut self,
        id: ResourceId,
        qty: u32,
        currency: &mut f32,
    ) -> (Option<Item>, u32) {
        let outcome = try_buy_inner(
            &mut self.listings,
            &mut self.market_stock,
            &self.prices,
            id,
            qty,
            currency,
        );
        record_bid_outcome(
            &outcome,
            id,
            qty,
            &mut self.bids_cleared,
            &mut self.bids_stockout,
            &mut self.bids_unaffordable,
        );
        match outcome {
            BuyOutcome::Cleared { item, qty: bought } => (Some(item), bought),
            _ => (None, 0),
        }
    }
}

impl Default for Market {
    fn default() -> Self {
        // Seed the 22 legacy goods with sane base prices so the SOLO /
        // fallback market starts with sensible posted prices. Floors are
        // a small numerical safety; there is no upper ceiling — buyer
        // wallets cap price organically.
        let seed_prices: [(fn() -> ResourceId, f32); 22] = [
            (core_ids::fruit, 1.0),
            (core_ids::meat, 1.2),
            (core_ids::grain, 0.8),
            (core_ids::wood, 0.8),
            (core_ids::stone, 0.5),
            (core_ids::tools, 2.0),
            (core_ids::cloth, 1.5),
            (core_ids::coal, 1.2),
            (core_ids::iron, 1.8),
            (core_ids::luxury, 5.0),
            (core_ids::grain_seed, 0.5),
            (core_ids::weapon, 3.0),
            (core_ids::armor, 4.0),
            (core_ids::shield, 2.5),
            (core_ids::skin, 0.7),
            (core_ids::copper, 2.0),
            (core_ids::tin, 2.5),
            (core_ids::gold, 25.0),
            (core_ids::silver, 10.0),
            (core_ids::berry_seed, 0.4),
            (core_ids::clay_tablet, 3.0),
            (core_ids::book, 8.0),
        ];

        let mut prices = AHashMap::new();
        let mut price_floor = AHashMap::new();
        let mut market_stock = AHashMap::new();
        for &(get_id, base_price) in &seed_prices {
            let id = get_id();
            prices.insert(id, base_price);
            price_floor.insert(id, DEFAULT_PRICE_FLOOR);
        }
        market_stock.insert(core_ids::tools(), 50.0);

        Self {
            prices,
            bids_cleared: AHashMap::new(),
            bids_stockout: AHashMap::new(),
            bids_unaffordable: AHashMap::new(),
            price_floor,
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

    /// Current price for the resource identified by `id`. Returns 1.0 if
    /// no entry exists yet.
    pub fn price_of(&self, id: ResourceId) -> f32 {
        self.price_id(id)
    }

    pub fn add_bid_cleared(&mut self, id: ResourceId, qty: f32) {
        *self.bids_cleared.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_bid_stockout(&mut self, id: ResourceId, qty: f32) {
        *self.bids_stockout.entry(id).or_insert(0.0) += qty;
    }

    pub fn add_bid_unaffordable(&mut self, id: ResourceId, qty: f32) {
        *self.bids_unaffordable.entry(id).or_insert(0.0) += qty;
    }

    pub fn stock_of(&self, id: ResourceId) -> f32 {
        self.market_stock.get(&id).copied().unwrap_or(0.0)
    }

    /// Bid-driven price update. See `Market` doc.
    pub fn update_prices(&mut self) {
        let mut active: Vec<ResourceId> = self.prices.keys().copied().collect();
        for id in self
            .bids_cleared
            .keys()
            .chain(self.bids_stockout.keys())
            .chain(self.bids_unaffordable.keys())
        {
            if !active.contains(id) {
                active.push(*id);
            }
        }
        for id in active {
            let cleared = self.bids_cleared.get(&id).copied().unwrap_or(0.0);
            let stockout = self.bids_stockout.get(&id).copied().unwrap_or(0.0);
            let unaffordable = self.bids_unaffordable.get(&id).copied().unwrap_or(0.0);
            let activity = cleared + stockout + unaffordable;
            if activity < 1.0 {
                continue;
            }
            let signal = (stockout - unaffordable) / activity;
            let cur = self.price_id(id);
            let next = (cur * (1.0 + signal * PRICE_DRIFT_PER_TICK)).max(self.floor_id(id));
            self.prices.insert(id, next);
        }
    }

    pub fn clear_flow(&mut self) {
        self.bids_cleared.clear();
        self.bids_stockout.clear();
        self.bids_unaffordable.clear();
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
            if let Some(entry) = self.listings.iter_mut().find(|(it, _)| *it == item) {
                entry.1 += qty;
            } else {
                self.listings.push((item, qty));
            }
        } else {
            *self.market_stock.entry(id).or_insert(0.0) += qty as f32;
        }
        total_earned
    }

    /// Agent attempts to buy. Records the outcome as a bid signal.
    pub fn try_buy_item(
        &mut self,
        id: ResourceId,
        qty: u32,
        currency: &mut f32,
    ) -> (Option<Item>, u32) {
        let outcome = try_buy_inner(
            &mut self.listings,
            &mut self.market_stock,
            &self.prices,
            id,
            qty,
            currency,
        );
        record_bid_outcome(
            &outcome,
            id,
            qty,
            &mut self.bids_cleared,
            &mut self.bids_stockout,
            &mut self.bids_unaffordable,
        );
        match outcome {
            BuyOutcome::Cleared { item, qty: bought } => (Some(item), bought),
            _ => (None, 0),
        }
    }
}

/// Result of a buy attempt — drives bid-signal bookkeeping.
enum BuyOutcome {
    /// Buy went through.
    Cleared { item: Item, qty: u32 },
    /// Market had no listings of this resource and no stock.
    Stockout,
    /// Listings or stock existed but price exceeded buyer's currency.
    Unaffordable,
}

/// Shared try-buy core. Mutates `listings`, `market_stock`, and `currency`
/// on success; reports outcome for the caller to record. Reads `prices`
/// via the same lookup as `price_id` (`unwrap_or(1.0)`).
fn try_buy_inner(
    listings: &mut Vec<(Item, u32)>,
    market_stock: &mut AHashMap<ResourceId, f32>,
    prices: &AHashMap<ResourceId, f32>,
    id: ResourceId,
    qty: u32,
    currency: &mut f32,
) -> BuyOutcome {
    let posted = prices.get(&id).copied().unwrap_or(1.0);
    let mut had_inventory = false; // any listings or stock for this id?

    // 1. Check listings (manufactured items) — buy best-multiplier affordable.
    let mut best_idx: Option<usize> = None;
    let mut best_mult = -1.0;
    for (idx, (item, stock)) in listings.iter().enumerate() {
        if item.resource_id == id && *stock > 0 {
            had_inventory = true;
            let price = posted * item.multiplier();
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
        let item = listings[idx].0;
        let price = posted * item.multiplier();
        let buy_qty = qty.min(listings[idx].1);
        let total_cost = price * buy_qty as f32;
        if total_cost <= *currency && buy_qty > 0 {
            *currency -= total_cost;
            listings[idx].1 -= buy_qty;
            return BuyOutcome::Cleared { item, qty: buy_qty };
        }
    }

    // 2. Fall through to commodity stock.
    let stock = market_stock.get(&id).copied().unwrap_or(0.0);
    if stock > 0.0 {
        had_inventory = true;
    }
    let buy_qty = (stock.floor() as u32).min(qty);
    if buy_qty == 0 {
        return if had_inventory {
            // Listings existed but were unaffordable; commodity stock empty.
            BuyOutcome::Unaffordable
        } else {
            BuyOutcome::Stockout
        };
    }
    let item = Item::new_commodity(id);
    let price = posted * item.multiplier();
    let total_cost = price * buy_qty as f32;
    if total_cost > *currency {
        return BuyOutcome::Unaffordable;
    }
    *currency -= total_cost;
    market_stock.insert(id, stock - buy_qty as f32);
    BuyOutcome::Cleared { item, qty: buy_qty }
}

fn record_bid_outcome(
    outcome: &BuyOutcome,
    id: ResourceId,
    requested_qty: u32,
    bids_cleared: &mut AHashMap<ResourceId, f32>,
    bids_stockout: &mut AHashMap<ResourceId, f32>,
    bids_unaffordable: &mut AHashMap<ResourceId, f32>,
) {
    match outcome {
        BuyOutcome::Cleared { qty, .. } => {
            *bids_cleared.entry(id).or_insert(0.0) += *qty as f32;
        }
        BuyOutcome::Stockout => {
            *bids_stockout.entry(id).or_insert(0.0) += requested_qty as f32;
        }
        BuyOutcome::Unaffordable => {
            *bids_unaffordable.entry(id).or_insert(0.0) += requested_qty as f32;
        }
    }
}

/// Run cadence for both price-update systems. Bid signals accumulate
/// over the window so trajectory is preserved at 1/PRICE_UPDATE_INTERVAL
/// the cost. Tick-rate sensitivity isn't a goal here — `update_prices`
/// nudges by ±5% per call regardless of interval.
const PRICE_UPDATE_INTERVAL: u64 = 5;

pub fn price_update_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mut market: ResMut<Market>,
    mode: Res<EconomicMode>,
) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    if clock.tick % PRICE_UPDATE_INTERVAL != 0 {
        return;
    }
    market.update_prices();
    market.clear_flow();
}

/// Pluralist Economy R7: per-settlement price update. Bid-driven —
/// no synthetic baseline demand. Walks every Settlement, ticks
/// `update_prices`, clears bid counters.
pub fn settlement_price_update_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mode: Res<EconomicMode>,
    mut settlements: Query<&mut crate::simulation::settlement::Settlement>,
) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    if clock.tick % PRICE_UPDATE_INTERVAL != 0 {
        return;
    }
    for mut settlement in settlements.iter_mut() {
        settlement.market.update_prices();
        settlement.market.clear_flow();
    }
}

/// P1b: per-camp price update. Mirrors `settlement_price_update_system`
/// for nomadic factions whose market lives on a `Camp` instead of a
/// `Settlement`. Same `PRICE_UPDATE_INTERVAL` cadence + `Command` mode
/// short-circuit so behaviour is uniform across archetypes.
pub fn camp_price_update_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mode: Res<EconomicMode>,
    mut camps: Query<&mut crate::simulation::camp::Camp>,
) {
    if matches!(*mode, EconomicMode::Command) {
        return;
    }
    if clock.tick % PRICE_UPDATE_INTERVAL != 0 {
        return;
    }
    for mut camp in camps.iter_mut() {
        camp.market.update_prices();
        camp.market.clear_flow();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_stable_with_no_buyer_activity() {
        let mut m = SettlementMarket::default();
        let fruit = core_ids::fruit();
        let initial = m.price_of(fruit);
        for _ in 0..200 {
            m.update_prices();
            m.clear_flow();
        }
        assert_eq!(m.price_of(fruit), initial);
    }

    #[test]
    fn price_rises_under_stockout() {
        let mut m = SettlementMarket::default();
        let fruit = core_ids::fruit();
        let initial = m.price_of(fruit);
        for _ in 0..50 {
            let mut wallet = 1000.0;
            let _ = m.try_buy_item(fruit, 5, &mut wallet);
            m.update_prices();
            m.clear_flow();
        }
        assert!(
            m.price_of(fruit) > initial,
            "expected price to rise under stockout: initial={initial}, now={}",
            m.price_of(fruit)
        );
    }

    #[test]
    fn price_falls_when_buyers_priced_out() {
        let mut m = SettlementMarket::default();
        let fruit = core_ids::fruit();
        // Force a high starting price via stockout pressure.
        for _ in 0..100 {
            let mut wallet = 1000.0;
            let _ = m.try_buy_item(fruit, 1, &mut wallet);
            m.update_prices();
            m.clear_flow();
        }
        let high = m.price_of(fruit);
        // Restock so subsequent failures are price-driven, not stockout-driven.
        m.set_stock(fruit, 1000.0);
        // Poor buyers can't afford the inflated price.
        for _ in 0..50 {
            let mut poor_wallet = 0.001;
            let _ = m.try_buy_item(fruit, 1, &mut poor_wallet);
            m.update_prices();
            m.clear_flow();
        }
        assert!(
            m.price_of(fruit) < high,
            "expected price to fall when buyers are priced out: high={high}, now={}",
            m.price_of(fruit)
        );
    }

    #[test]
    fn autumn_glut_does_not_crash_prices() {
        let mut m = SettlementMarket::default();
        let grain = core_ids::grain();
        let initial = m.price_of(grain);
        // Farmers harvest — stock fills up. No buyer activity yet.
        m.set_stock(grain, 10000.0);
        for _ in 0..200 {
            m.update_prices();
            m.clear_flow();
        }
        assert_eq!(m.price_of(grain), initial);
    }

    #[test]
    fn no_synthetic_pump_at_game_start() {
        let mut m = SettlementMarket::default();
        let fruit = core_ids::fruit();
        let initial = m.price_of(fruit);
        // 200 ticks of `settlement_price_update_system`-equivalent work
        // on a fresh, untouched market: no buyer activity → no signal →
        // no drift. (Regression test for the old baseline-demand pump.)
        for _ in 0..200 {
            m.update_prices();
            m.clear_flow();
        }
        assert_eq!(m.price_of(fruit), initial);
    }
}
