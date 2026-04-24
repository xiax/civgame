use bevy::prelude::*;

mod world;
mod simulation;
mod economy;
mod pathfinding;
mod rendering;
mod ui;
mod sandbox;

fn main() {
    let is_sandbox = std::env::args().any(|a| a == "--sandbox");

    let title = if is_sandbox { "CivGame [sandbox]" } else { "CivGame" };

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: title.into(),
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
        .insert_resource(Time::<Fixed>::from_hz(20.0));

    if is_sandbox {
        app.add_plugins(sandbox::SandboxPlugin);
    }

    app.run();
}
