use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::economy::market::Market;
use crate::economy::command::CommandPools;
use crate::economy::mode::EconomicMode;
use crate::economy::goods::{Good, GOOD_COUNT};

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

            match *mode {
                EconomicMode::Market | EconomicMode::Mixed => {
                    ui.label("Market Prices (Commodities):");
                    for good in Good::all() {
                        let price = market.prices[good as usize];
                        ui.label(format!(
                            "  {:6}: ${:.2}",
                            good.name(), price
                        ));
                    }
                    if !market.listings.is_empty() {
                        ui.separator();
                        ui.label("Item Listings:");
                        for (item, stock) in &market.listings {
                            if *stock > 0 {
                                let mut name = item.good.name().to_string();
                                if let Some(mat) = item.material { name = format!("{:?} {}", mat, name); }
                                if let Some(qual) = item.quality { name = format!("{} ({:?})", name, qual); }
                                ui.label(format!("  {} x{}: ${:.1}", name, stock, market.calculate_price(item)));
                            }
                        }
                    }
                }
                EconomicMode::Command => {
                    ui.label("Command Stockpiles:");
                    for good in Good::all() {
                        let stock = pools.stockpile[good as usize];
                        let quota = pools.quotas[good as usize];
                        ui.label(format!(
                            "  {:6}: {:.0} units  ({} workers)",
                            good.name(), stock, quota
                        ));
                    }
                }
            }
        });
}
