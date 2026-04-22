use bevy::prelude::*;

mod world;
mod simulation;
mod economy;
mod pathfinding;
mod rendering;
mod ui;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CivGame".into(),
                resolution: (1280.0, 720.0).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(world::WorldPlugin)
        .add_plugins(simulation::SimulationPlugin)
        .add_plugins(economy::EconomyPlugin)
        .add_plugins(pathfinding::PathfindingPlugin)
        .add_plugins(rendering::RenderingPlugin)
        .add_plugins(ui::UiPlugin)
        .run();
}
