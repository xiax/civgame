use bevy::prelude::*;
use crate::simulation::SimulationSet;

pub mod goods;
pub mod market;
pub mod command;
pub mod agent;
pub mod mode;
pub mod transactions;

pub use goods::{Good, GOOD_COUNT};
pub use market::Market;
pub use command::CommandPools;
pub use agent::EconomicAgent;
pub use mode::EconomicMode;

pub struct EconomyPlugin;

impl Plugin for EconomyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Market::default())
            .insert_resource(CommandPools::default())
            .insert_resource(EconomicMode::default());
    }
}
