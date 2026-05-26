use bevy::prelude::*;

mod economy;
mod game_state;
mod net;
mod net_id;
mod pathfinding;
mod rendering;
mod sandbox;
mod simulation;
mod ui;
mod world;

pub use game_state::{
    EconomyPreset, GameStartOptions, GameState, GameStatePlugin, PendingSpawn, PendingStarts,
    PlayerStartSlot, RegenerateWorldRequest, SimulationState, WorldSeed,
    HOST_SERVER_LOCAL_CLIENT_ID,
};

fn main() {
    let is_sandbox = std::env::args().any(|a| a == "--sandbox");

    let net_cfg = match net::parse_from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("CLI error: {}", err);
            std::process::exit(2);
        }
    };

    let title = match (is_sandbox, net_cfg.mode) {
        (true, _) => "CivGame [sandbox]".to_string(),
        (_, net::NetMode::ListenServer) => "CivGame [listen]".to_string(),
        (_, net::NetMode::DedicatedServer) => "CivGame [server]".to_string(),
        (_, net::NetMode::Client) => format!(
            "CivGame [client → {}]",
            net_cfg
                .connect_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "?".into())
        ),
        _ => "CivGame".to_string(),
    };

    // DedicatedServer runs the sim headlessly — no window, no rendering, no
    // UI. Every other mode (Local / ListenServer / Client) keeps a window.
    // ListenServer + Client also keep render/UI; the client App is what the
    // user sees and the listen-server host wants to play the game too.
    let is_headless = matches!(net_cfg.mode, net::NetMode::DedicatedServer);

    let mut app = App::new();
    // Pre-insert NetMode so NetPlugin's init_resource doesn't overwrite our
    // CLI choice. `NetConfig` itself becomes the canonical source for the
    // bind / connect / player-name fields Phase 2 transports will read.
    app.insert_resource(net_cfg.mode);
    app.insert_resource(net_cfg.on_disconnect);
    app.insert_resource(net_cfg.clone());

    if is_headless {
        // Headless: MinimalPlugins gives us TaskPool + Time +
        // ScheduleRunnerPlugin (which drives the schedule outside a windowed
        // event loop). LogPlugin keeps `info!` going to stderr. The fixed
        // timestep below sets sim cadence; ScheduleRunnerPlugin defaults to
        // a continuous loop, which suits a server with no frame cap.
        app.add_plugins(MinimalPlugins);
        app.add_plugins(bevy::log::LogPlugin::default());
    } else {
        app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: title.into(),
                resolution: (1280.0, 720.0).into(),
                ..default()
            }),
            ..default()
        }));
    }

    app.add_plugins(GameStatePlugin)
    .insert_resource(simulation::region::SettledRegions::default())
    .insert_resource(simulation::region::SimulationFocus::default())
    .add_plugins(net_id::NetIdPlugin)
    .add_plugins(net::NetPlugin)
    .add_plugins(world::WorldPlugin)
    .add_plugins(simulation::SimulationPlugin)
    .add_plugins(economy::EconomyPlugin)
    .add_plugins(pathfinding::PathfindingPlugin)
    .insert_resource(Time::<Fixed>::from_hz(20.0))
    .add_systems(Startup, configure_time)
    .add_systems(PreUpdate, log_first_pre_update)
    .add_systems(Update, log_first_update)
    .add_systems(Update, log_first_update_end.after(log_first_update))
    .add_systems(PostUpdate, log_first_post_update)
    .add_systems(
        PostUpdate,
        log_first_post_update_end.after(log_first_post_update),
    );

    if !is_headless {
        // Render + UI piggyback on `DefaultPlugins` (asset server, window,
        // winit). Both reference graphics resources DedicatedServer doesn't
        // have, so they're skipped under MinimalPlugins.
        app.add_plugins(rendering::RenderingPlugin)
            .add_plugins(ui::UiPlugin);
    }

    if is_sandbox {
        app.add_plugins(sandbox::SandboxPlugin);
    }

    app.run();
}

fn configure_time(mut time: ResMut<Time<Virtual>>) {
    time.set_max_delta(std::time::Duration::from_millis(50));
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
