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
) {
    egui::Window::new("Economy")
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
                        let price = market.price_of(id);
                        ui.label(format!(
                            "  {:6}: ${:.2}",
                            crate::economy::core_ids::display_name(id),
                            price
                        ));
                    }
                    if !market.listings.is_empty() {
                        ui.separator();
                        ui.label("Item Listings:");
                        for (item, stock) in &market.listings {
                            if *stock > 0 {
                                let name = item.label();
                                ui.label(format!(
                                    "  {} x{}: ${:.1}",
                                    name,
                                    stock,
                                    market.calculate_price(item)
                                ));
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
