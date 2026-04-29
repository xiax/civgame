use super::agent::EconomicAgent;
use super::goods::Good;
use super::market::Market;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use bevy::prelude::*;

const FOOD_KEEP_RESERVE: u32 = 2;
const HUNGER_BUY_THRESHOLD: u8 = 170;
const TOOL_BUY_CURRENCY_FACTOR: f32 = 1.5;

pub fn market_sell_system(
    clock: Res<SimClock>,
    mut market: ResMut<Market>,
    mut query: Query<(&PersonAI, &mut EconomicAgent, &BucketSlot, &LodLevel)>,
) {
    for (ai, mut agent, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Idle {
            continue;
        }

        // Sell all items except food reserve
        let inventory = agent.inventory; // Copy to avoid borrow issues while mutably removing
        for (item, qty) in inventory {
            if qty == 0 {
                continue;
            }

            let sell_qty = if item.good.is_edible() {
                if qty > FOOD_KEEP_RESERVE {
                    qty - FOOD_KEEP_RESERVE
                } else {
                    0
                }
            } else {
                qty
            };

            if sell_qty > 0 {
                let earned = market.sell_item(item, sell_qty);
                agent.remove_item(item, sell_qty);
                agent.currency += earned;
            }
        }
    }
}

pub fn market_buy_system(
    clock: Res<SimClock>,
    mut market: ResMut<Market>,
    mut query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &Needs,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, needs, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        // Buy Food when hungry and have no food
        if needs.hunger > HUNGER_BUY_THRESHOLD as f32 && agent.total_food() == 0 {
            let (bought_item, qty) = market.try_buy_item(Good::Fruit, 1, &mut agent.currency);
            if let Some(it) = bought_item {
                agent.add_item(it, qty);
                // Clear Trader task now that the buy was handled
                if ai.task_id == crate::simulation::tasks::TaskKind::Trader as u16 {
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                }
            }
        }

        // Buy Tools when affordable and not already owning one
        if !agent.has_tool() {
            let tool_price = market.prices[Good::Tools as usize];
            if agent.currency >= tool_price * TOOL_BUY_CURRENCY_FACTOR {
                let (bought_item, qty) = market.try_buy_item(Good::Tools, 1, &mut agent.currency);
                if let Some(it) = bought_item {
                    agent.add_item(it, qty);
                }
            }
        }
    }
}
