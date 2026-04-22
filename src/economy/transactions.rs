use bevy::prelude::*;
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use super::agent::EconomicAgent;
use super::goods::Good;
use super::market::Market;

const FOOD_KEEP_RESERVE: u8 = 2;
const HUNGER_BUY_THRESHOLD: u8 = 130;
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

        // Sell surplus Food (keep reserve)
        let food_qty = agent.quantity_of(Good::Food);
        if food_qty > FOOD_KEEP_RESERVE {
            let sell_qty = food_qty - FOOD_KEEP_RESERVE;
            let earned = market.sell(Good::Food, sell_qty as f32);
            agent.remove_good(Good::Food, sell_qty);
            agent.currency += earned;
        }

        // Sell all Wood
        let wood_qty = agent.quantity_of(Good::Wood);
        if wood_qty > 0 {
            let earned = market.sell(Good::Wood, wood_qty as f32);
            agent.remove_good(Good::Wood, wood_qty);
            agent.currency += earned;
        }

        // Sell all Stone
        let stone_qty = agent.quantity_of(Good::Stone);
        if stone_qty > 0 {
            let earned = market.sell(Good::Stone, stone_qty as f32);
            agent.remove_good(Good::Stone, stone_qty);
            agent.currency += earned;
        }

        // Sell Coal and Iron
        for good in [Good::Coal, Good::Iron] {
            let qty = agent.quantity_of(good);
            if qty > 0 {
                let earned = market.sell(good, qty as f32);
                agent.remove_good(good, qty);
                agent.currency += earned;
            }
        }
    }
}

pub fn market_buy_system(
    clock: Res<SimClock>,
    mut market: ResMut<Market>,
    mut query: Query<(&mut PersonAI, &mut EconomicAgent, &Needs, &BucketSlot, &LodLevel)>,
) {
    for (mut ai, mut agent, needs, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }

        // Buy Food when hungry and have no food
        if needs.hunger > HUNGER_BUY_THRESHOLD && agent.quantity_of(Good::Food) == 0 {
            let bought = market.try_buy(Good::Food, 1.0, &mut agent.currency);
            if bought > 0.0 {
                agent.add_good(Good::Food, 1);
                // Clear Trader job now that the buy was handled
                if ai.job_id == crate::simulation::jobs::JobKind::Trader as u16 {
                    ai.state = AiState::Idle;
                    ai.job_id = PersonAI::UNEMPLOYED;
                }
            }
        }

        // Buy Tools when affordable and not already owning one
        if !agent.has_tool() {
            let tool_price = market.prices[Good::Tools as usize];
            if agent.currency >= tool_price * TOOL_BUY_CURRENCY_FACTOR {
                let bought = market.try_buy(Good::Tools, 1.0, &mut agent.currency);
                if bought > 0.0 {
                    agent.add_good(Good::Tools, 1);
                }
            }
        }
    }
}
