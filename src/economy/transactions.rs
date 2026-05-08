use super::agent::EconomicAgent;
use super::market::Market;
use crate::simulation::faction::FactionMember;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::settlement::{Settlement, SettlementMap};
use bevy::prelude::*;

/// Pluralist Economy R10: trader buys `qty` of `resource_id` from
/// the named settlement's market at the market's current price.
/// Updates settlement.treasury (credit), agent.currency (debit), and
/// the settlement market's commodity stock (decrement). Returns the
/// per-unit price actually paid on success, or None on insufficient
/// funds / insufficient stock / settlement missing.
///
/// Currency invariant: every dollar leaving the agent enters the
/// settlement treasury — no money is created or destroyed. Stock
/// invariant: every unit leaving the market lands in the agent's
/// inventory.
pub fn trader_buy_at_settlement(
    world: &mut World,
    trader: Entity,
    settlement: Entity,
    resource_id: crate::economy::resource_catalog::ResourceId,
    qty: u32,
) -> Option<f32> {
    if qty == 0 {
        return None;
    }
    // Read settlement state for the price + stock check.
    let (price_per_unit, stock_available) = {
        let s = world
            .get::<crate::simulation::settlement::Settlement>(settlement)?;
        (s.market.price_of(resource_id), s.market.stock_of(resource_id))
    };
    let total = price_per_unit * qty as f32;
    if stock_available < qty as f32 {
        return None;
    }
    // Currency check.
    let agent_currency = world.get::<EconomicAgent>(trader)?.currency;
    if agent_currency < total {
        return None;
    }
    // Capacity check via add_item dry-run is awkward; we just attempt
    // the add and roll back the debit if the agent can't carry it.
    let item = crate::economy::item::Item::new_commodity(resource_id);
    let leftover = world
        .get_mut::<EconomicAgent>(trader)?
        .add_item(item, qty);
    let acquired = qty - leftover;
    if acquired == 0 {
        return None;
    }
    // Debit agent currency for the actually-acquired qty.
    let actual_total = price_per_unit * acquired as f32;
    {
        let mut econ = world.get_mut::<EconomicAgent>(trader)?;
        econ.currency -= actual_total;
    }
    // Update settlement: stock down, treasury up, demand up so the
    // price tick reflects the trade.
    {
        let mut s = world
            .get_mut::<crate::simulation::settlement::Settlement>(settlement)?;
        s.market.add_demand(resource_id, acquired as f32);
        // Decrement stock by drawing through `try_buy_item`'s
        // commodity branch — but we already debited the agent, so we
        // bypass the helper's currency mutation by manipulating the
        // sparse map directly through a fresh `add_supply` of the
        // negative qty would be wrong. Instead, sell-side: we mirror
        // `try_buy_item`'s commodity-stock side-effect manually.
        let new_stock = (stock_available - acquired as f32).max(0.0);
        // SettlementMarket exposes `stock_of` and `add_supply` but
        // not a direct stock setter; route through `add_supply` with
        // the negative delta. Add a small public mutator for this.
        s.market.set_stock(resource_id, new_stock);
        s.treasury += actual_total;
    }
    Some(price_per_unit)
}

/// Pluralist Economy R10: trader sells `qty` of `resource_id` at the
/// named settlement's market at the market's current price. Settlement
/// treasury is debited (or capped at 0 if insufficient — the trader
/// receives only what the treasury can pay). Returns the per-unit
/// price actually received on success, or None on missing inventory /
/// missing settlement.
pub fn trader_sell_at_settlement(
    world: &mut World,
    trader: Entity,
    settlement: Entity,
    resource_id: crate::economy::resource_catalog::ResourceId,
    qty: u32,
) -> Option<f32> {
    if qty == 0 {
        return None;
    }
    let agent_qty = world.get::<EconomicAgent>(trader)?.quantity_of_resource(resource_id);
    if agent_qty < qty {
        return None;
    }
    let price_per_unit = {
        let s = world
            .get::<crate::simulation::settlement::Settlement>(settlement)?;
        s.market.price_of(resource_id)
    };
    let asking = price_per_unit * qty as f32;
    let treasury_available = world
        .get::<crate::simulation::settlement::Settlement>(settlement)?
        .treasury;
    let actual_payout = asking.min(treasury_available);
    let actual_qty = if asking > 0.0 {
        ((actual_payout / price_per_unit).floor() as u32).min(qty)
    } else {
        0
    };
    if actual_qty == 0 {
        return None;
    }
    let actual_total = price_per_unit * actual_qty as f32;
    // Agent: remove inventory, credit currency. Pluralist Economy
    // R6 follow-on b: when the trader is a household member, split
    // earnings via `split_market_earnings_with_household` —
    // household treasury skims `HOUSEHOLD_INCOME_SKIM`; the rest
    // goes to the agent's wallet.
    let agent_share = split_market_earnings_with_household(world, trader, actual_total);
    {
        let mut econ = world.get_mut::<EconomicAgent>(trader)?;
        econ.remove_resource(resource_id, actual_qty);
        econ.currency += agent_share;
    }
    // Settlement: treasury down, market stock up, supply up.
    {
        let mut s = world
            .get_mut::<crate::simulation::settlement::Settlement>(settlement)?;
        s.treasury -= actual_total;
        if s.treasury < 0.0 {
            s.treasury = 0.0;
        }
        let cur_stock = s.market.stock_of(resource_id);
        s.market.set_stock(resource_id, cur_stock + actual_qty as f32);
        s.market.add_supply(resource_id, actual_qty as f32);
    }
    Some(price_per_unit)
}

/// Atomic agent-to-agent currency transfer. Returns false if `amount` is
/// non-positive, `from` has insufficient funds, or either entity lacks
/// an `EconomicAgent`. On success, currency is debited from `from` and
/// credited to `to` in the same call — no observer can see a state
/// where the system-wide currency invariant is broken.
///
/// Pluralist Economy R2: this is the **only** way agents pay each
/// other. Wages, tribute, escrow refunds, and contract payments all
/// go through here.
pub fn pay(world: &mut World, from: Entity, to: Entity, amount: f32) -> bool {
    if !(amount > 0.0) {
        return false;
    }
    let from_balance = match world.get::<EconomicAgent>(from) {
        Some(a) => a.currency,
        None => return false,
    };
    if from_balance < amount {
        return false;
    }
    if world.get::<EconomicAgent>(to).is_none() {
        return false;
    }
    if let Some(mut from_agent) = world.get_mut::<EconomicAgent>(from) {
        from_agent.currency -= amount;
    }
    if let Some(mut to_agent) = world.get_mut::<EconomicAgent>(to) {
        to_agent.currency += amount;
    }
    true
}

const FOOD_KEEP_RESERVE: u32 = 2;
const HUNGER_BUY_THRESHOLD: u8 = 170;
const TOOL_BUY_CURRENCY_FACTOR: f32 = 1.5;

/// Pluralist Economy R6 follow-on b: fraction of a household
/// member's market sale earnings that goes to the household
/// treasury rather than their personal wallet. Without this,
/// households can't accumulate treasury organically and
/// `household_contract_posting_system` never fires outside tests
/// that pre-seed the treasury.
pub const HOUSEHOLD_INCOME_SKIM: f32 = 0.10;

/// Pluralist Economy R6 follow-on b: split market-sale earnings
/// between an agent's wallet and their household treasury (when
/// they're a household member). Returns the agent's share. Caller
/// adds the agent's share to `EconomicAgent.currency` themselves;
/// the household-treasury credit happens here through the
/// `FactionRegistry` resource. Currency invariant: the function
/// debits nothing (the caller hasn't credited anything yet); it
/// only redirects part of the would-be agent credit to the
/// household.
///
/// Returns the **agent's share** of `earned`. If the agent is not
/// a household member, returns `earned` unchanged. If the agent
/// has no `EconomicAgent` (defensive), returns 0.
pub fn split_market_earnings_with_household(
    world: &mut World,
    agent: Entity,
    earned: f32,
) -> f32 {
    if earned <= 0.0 {
        return 0.0;
    }
    let household_id = world
        .get::<crate::simulation::reproduction::HouseholdMember>(agent)
        .map(|hm| hm.household_id);
    let Some(household_id) = household_id else {
        return earned;
    };
    let skim = earned * HOUSEHOLD_INCOME_SKIM;
    if let Some(mut registry) = world.get_resource_mut::<crate::simulation::faction::FactionRegistry>() {
        if let Some(hh) = registry.factions.get_mut(&household_id) {
            hh.treasury += skim;
        }
    }
    earned - skim
}

/// Pluralist Economy R7: route an agent's market interaction to
/// their faction's first settlement market when one exists,
/// otherwise fall back to the global `Market`. SOLO and unsettled
/// agents always hit the global fallback.
fn settlement_for(
    settlement_map: &SettlementMap,
    member: &FactionMember,
) -> Option<crate::simulation::settlement::SettlementId> {
    if member.faction_id == crate::simulation::faction::SOLO {
        return None;
    }
    settlement_map.first_for_faction(member.faction_id)
}

pub fn market_sell_system(
    clock: Res<SimClock>,
    mut market: ResMut<Market>,
    settlement_map: Res<SettlementMap>,
    mut settlements: Query<&mut Settlement>,
    mut faction_registry: ResMut<crate::simulation::faction::FactionRegistry>,
    mut query: Query<(
        &PersonAI,
        &mut EconomicAgent,
        &BucketSlot,
        &LodLevel,
        &FactionMember,
        // Pluralist Economy R6 follow-on b: household members
        // skim a fraction of market earnings into the household
        // treasury.
        Option<&crate::simulation::reproduction::HouseholdMember>,
    )>,
) {
    // Pluralist Economy R6 follow-on b: household-skim intents
    // collected during the iter loop; applied to FactionRegistry
    // after the iter releases its mutable borrows.
    let mut skim_intents: Vec<(u32, f32)> = Vec::new();

    for (ai, mut agent, slot, lod, member, household_opt) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Idle {
            continue;
        }

        // R7: pick the right market to trade against. The settlement
        // entity may be missing if `auto_found_default_settlements_system`
        // hasn't run yet; in that case treat as unsettled.
        let settlement_entity = settlement_for(&settlement_map, member)
            .and_then(|sid| settlement_map.by_id.get(&sid).copied());

        // Sell all items except food reserve
        let inventory = agent.inventory; // Copy to avoid borrow issues while mutably removing
        for (item, qty) in inventory {
            if qty == 0 {
                continue;
            }

            let sell_qty = if item.resource_id.is_edible() {
                if qty > FOOD_KEEP_RESERVE {
                    qty - FOOD_KEEP_RESERVE
                } else {
                    0
                }
            } else {
                qty
            };

            if sell_qty > 0 {
                let earned = match settlement_entity
                    .and_then(|e| settlements.get_mut(e).ok())
                {
                    Some(mut s) => s.market.sell_item(item, sell_qty),
                    None => market.sell_item(item, sell_qty),
                };
                agent.remove_item(item, sell_qty);
                // R6 follow-on b: split earnings between agent and
                // household treasury when the agent is a household
                // member. Currency invariant: skim leaves the
                // agent → enters the household treasury → total
                // unchanged.
                let skim = match household_opt {
                    Some(hm) if earned > 0.0 => {
                        let s = earned * HOUSEHOLD_INCOME_SKIM;
                        skim_intents.push((hm.household_id, s));
                        s
                    }
                    _ => 0.0,
                };
                agent.currency += earned - skim;
            }
        }
    }

    // Apply household treasury skims after the query releases its
    // borrow on EconomicAgent.
    for (household_id, skim) in skim_intents {
        if let Some(hh) = faction_registry.factions.get_mut(&household_id) {
            hh.treasury += skim;
        }
        // If the household disappeared (despawned mid-loop), the
        // skim is lost — same semantics as a refund-with-vanished-
        // beneficiary in JobEscrow's on_remove. Currency invariant
        // tolerates this drift in the same way.
    }
}

pub fn market_buy_system(
    clock: Res<SimClock>,
    mut market: ResMut<Market>,
    settlement_map: Res<SettlementMap>,
    mut settlements: Query<&mut Settlement>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &Needs,
        &BucketSlot,
        &LodLevel,
        &FactionMember,
    )>,
) {
    for (mut ai, mut agent, needs, slot, lod, member) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        let settlement_entity = settlement_for(&settlement_map, member)
            .and_then(|sid| settlement_map.by_id.get(&sid).copied());

        // Buy Food when hungry and have no food
        if needs.hunger > HUNGER_BUY_THRESHOLD as f32 && agent.total_food() == 0 {
            let (bought_item, qty) = match settlement_entity
                .and_then(|e| settlements.get_mut(e).ok())
            {
                Some(mut s) => s.market.try_buy_item(
                    crate::economy::core_ids::fruit(),
                    1,
                    &mut agent.currency,
                ),
                None => market.try_buy_item(
                    crate::economy::core_ids::fruit(),
                    1,
                    &mut agent.currency,
                ),
            };
            if let Some(it) = bought_item {
                agent.add_item(it, qty);
                if ai.task_id == crate::simulation::tasks::TaskKind::Trader as u16 {
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
            }
        }

        // Buy Tools when affordable and not already owning one
        if !agent.has_tool() {
            let tools_id = crate::economy::core_ids::tools();
            let tool_price = match settlement_entity.and_then(|e| settlements.get(e).ok()) {
                Some(s) => s.market.price_of(tools_id),
                None => market.price_of(tools_id),
            };
            if agent.currency >= tool_price * TOOL_BUY_CURRENCY_FACTOR {
                let (bought_item, qty) = match settlement_entity
                    .and_then(|e| settlements.get_mut(e).ok())
                {
                    Some(mut s) => s.market.try_buy_item(tools_id, 1, &mut agent.currency),
                    None => market.try_buy_item(tools_id, 1, &mut agent.currency),
                };
                if let Some(it) = bought_item {
                    agent.add_item(it, qty);
                }
            }
        }
    }
}
