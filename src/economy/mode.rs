use bevy::prelude::*;

#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EconomicMode {
    #[default]
    Market,
    Command,
    Mixed,
}

impl EconomicMode {
    pub fn label(self) -> &'static str {
        match self {
            EconomicMode::Market  => "Market Economy",
            EconomicMode::Command => "Command Economy",
            EconomicMode::Mixed   => "Mixed Economy",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            EconomicMode::Market  => EconomicMode::Mixed,
            EconomicMode::Mixed   => EconomicMode::Command,
            EconomicMode::Command => EconomicMode::Market,
        }
    }
}
