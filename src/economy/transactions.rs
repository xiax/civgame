use super::agent::EconomicAgent;
use super::market::Market;
use crate::simulation::faction::FactionMember;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::settlement::{Settlement, SettlementMap};
use bevy::prelude::*;

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
    mut query: Query<(
        &PersonAI,
        &mut EconomicAgent,
        &BucketSlot,
        &LodLevel,
        &FactionMember,
    )>,
) {
    for (ai, mut agent, slot, lod, member) in query.iter_mut() {
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
                agent.currency += earned;
            }
        }
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
