use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::command::CommandPools;
use crate::economy::market::Market;
use crate::economy::mode::EconomicMode;

pub fn economy_panel_system(
    mut contexts: EguiContexts,
    market: Res<Market>,
    pools: Res<CommandPools>,
    mode: Res<EconomicMode>,
    player_faction: Res<crate::simulation::faction::PlayerFaction>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlements: Query<&crate::simulation::settlement::Settlement>,
) {
    // Pluralist Economy R7: prefer the player faction's first
    // settlement market over the global one. Falls back to global
    // when the player has no settlements yet (start-up edge case).
    let player_settlement = settlement_map
        .first_for_faction(player_faction.faction_id)
        .and_then(|sid| settlement_map.by_id.get(&sid).copied())
        .and_then(|e| settlements.get(e).ok());
    let title = if player_settlement.is_some() {
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
                        let price = match player_settlement {
                            Some(s) => s.market.price_of(id),
                            None => market.price_of(id),
                        };
                        ui.label(format!(
                            "  {:6}: ${:.2}",
                            crate::economy::core_ids::display_name(id),
                            price
                        ));
                    }
                    let listings: &[(crate::economy::item::Item, u32)] = match player_settlement
                    {
                        Some(s) => &s.market.listings,
                        None => &market.listings,
                    };
                    if !listings.is_empty() {
                        ui.separator();
                        ui.label("Item Listings:");
                        for (item, stock) in listings {
                            if *stock > 0 {
                                let name = item.label();
                                let price = match player_settlement {
                                    Some(s) => s.market.calculate_price(item),
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
