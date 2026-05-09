use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::command::CommandPools;
use crate::economy::market::{Market, SettlementMarket};
use crate::economy::mode::EconomicMode;
use crate::simulation::camp::{faction_market_node, Camp, CampMap, MarketNodeRef};
use crate::simulation::settlement::{Settlement, SettlementMap};

pub fn economy_panel_system(
    mut contexts: EguiContexts,
    market: Res<Market>,
    pools: Res<CommandPools>,
    mode: Res<EconomicMode>,
    player_faction: Res<crate::simulation::faction::PlayerFaction>,
    settlement_map: Res<SettlementMap>,
    camp_map: Res<CampMap>,
    settlements: Query<&Settlement, Without<Camp>>,
    camps: Query<&Camp, Without<Settlement>>,
) {
    // P1b: prefer the player faction's economic node — Settlement for
    // settled archetypes, Camp for nomadic. Falls back to the global
    // Market when no node exists yet (start-up edge case).
    let player_market: Option<&SettlementMarket> =
        match faction_market_node(&settlement_map, &camp_map, player_faction.faction_id) {
            Some(MarketNodeRef::Settlement(e)) => settlements.get(e).ok().map(|s| &s.market),
            Some(MarketNodeRef::Camp(e)) => camps.get(e).ok().map(|c| &c.market),
            None => None,
        };
    let title = if player_market.is_some() {
        "Economy (Local Market)"
    } else {
        "Economy"
    };
    egui::Window::new(title)
        .default_pos([10.0, 400.0])
        .default_width(220.0)
        .show(contexts.ctx_mut(), |ui| {
            ui.label(format!("Mode: {}", mode.label()));
            ui.separator();

            let catalog = crate::economy::core_ids::catalog();
            match *mode {
                EconomicMode::Market | EconomicMode::Mixed => {
                    ui.label("Market Prices (Commodities):");
                    for (id, _def) in catalog.iter() {
                        let price = match player_market {
                            Some(m) => m.price_of(id),
                            None => market.price_of(id),
                        };
                        ui.label(format!(
                            "  {:6}: ${:.2}",
                            crate::economy::core_ids::display_name(id),
                            price
                        ));
                    }
                    let listings: &[(crate::economy::item::Item, u32)] = match player_market {
                        Some(m) => &m.listings,
                        None => &market.listings,
                    };
                    if !listings.is_empty() {
                        ui.separator();
                        ui.label("Item Listings:");
                        for (item, stock) in listings {
                            if *stock > 0 {
                                let name = item.label();
                                let price = match player_market {
                                    Some(m) => m.calculate_price(item),
                                    None => market.calculate_price(item),
                                };
                                ui.label(format!("  {} x{}: ${:.1}", name, stock, price));
                            }
                        }
                    }
                }
                EconomicMode::Command => {
                    ui.label("Command Stockpiles:");
                    for (id, _def) in catalog.iter() {
                        let idx = id.0 as usize;
                        if idx >= pools.stockpile.len() {
                            continue;
                        }
                        let stock = pools.stockpile[idx];
                        let quota = pools.quotas[idx];
                        ui.label(format!(
                            "  {:6}: {:.0} units  ({} workers)",
                            crate::economy::core_ids::display_name(id),
                            stock,
                            quota
                        ));
                    }
                }
            }
        });
}
