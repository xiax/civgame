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
                    ui.label("Market Prices:");
                    for good in Good::all() {
                        let price = market.prices[good as usize];
                        let supply = market.supply[good as usize];
                        let demand = market.demand[good as usize];
                        ui.label(format!(
                            "  {:6}: ${:.2}  S:{:.0} D:{:.0}",
                            good.name(), price, supply, demand
                        ));
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
