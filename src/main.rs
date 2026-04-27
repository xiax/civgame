use bevy::prelude::*;

mod economy;
mod pathfinding;
mod rendering;
mod sandbox;
mod simulation;
mod ui;
mod world;

fn main() {
    let is_sandbox = std::env::args().any(|a| a == "--sandbox");

    let title = if is_sandbox {
        "CivGame [sandbox]"
    } else {
        "CivGame"
    };

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
    .insert_resource(Time::<Fixed>::from_hz(20.0))
    .add_systems(PreUpdate, log_first_pre_update)
    .add_systems(Update, log_first_update)
    .add_systems(Update, log_first_update_end.after(log_first_update))
    .add_systems(PostUpdate, log_first_post_update)
    .add_systems(
        PostUpdate,
        log_first_post_update_end.after(log_first_post_update),
    );

    if is_sandbox {
        app.add_plugins(sandbox::SandboxPlugin);
    }

    app.run();
}

fn log_first_pre_update(mut has_logged: Local<bool>, time: Res<Time>) {
    if !*has_logged {
        info!("First PreUpdate frame reached in {:?}", time.elapsed());
        *has_logged = true;
    }
}

fn log_first_update(mut has_logged: Local<bool>, time: Res<Time>) {
    if !*has_logged {
        info!("First Update frame reached in {:?}", time.elapsed());
        *has_logged = true;
    }
}

fn log_first_update_end(mut has_logged: Local<bool>, time: Res<Time>) {
    if !*has_logged {
        info!("First Update frame finished in {:?}", time.elapsed());
        *has_logged = true;
    }
}

fn log_first_post_update(mut has_logged: Local<bool>, time: Res<Time>) {
    if !*has_logged {
        info!("First PostUpdate frame reached in {:?}", time.elapsed());
        *has_logged = true;
    }
}

fn log_first_post_update_end(mut has_logged: Local<bool>, time: Res<Time>) {
    if !*has_logged {
        info!("First PostUpdate frame finished in {:?}", time.elapsed());
        *has_logged = true;
    }
}
