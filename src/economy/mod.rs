use crate::simulation::SimulationSet;
use bevy::prelude::*;

pub mod agent;
pub mod command;
pub mod goods;
pub mod item;
pub mod market;
pub mod mode;
pub mod transactions;

pub use command::CommandPools;
pub use market::Market;
pub use mode::EconomicMode;

pub struct EconomyPlugin;

impl Plugin for EconomyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Market::default())
            .insert_resource(CommandPools::default())
            .insert_resource(EconomicMode::default())
            .add_systems(
                FixedUpdate,
                (market::price_update_system,).in_set(SimulationSet::Economy),
            );
    }
}
