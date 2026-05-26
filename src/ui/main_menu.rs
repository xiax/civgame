//! Title screen with three buttons (Singleplayer, Host LAN Game, Join LAN
//! Game). Renders only in `GameState::MainMenu`. On click, populates the
//! singleplayer / multiplayer init resources and transitions to the
//! appropriate next state.
//!
//! Host/Join in v1 use re-launch (see `ui::lobby` + `net::cli`); in
//! `NetMode::Local` they currently route into the lobby in
//! "preview" mode so the UI can be exercised single-process. The
//! actual `--listen` / `--connect` flag wiring lands in Commit 5.

use std::process::Command;

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::game_state::{GameState, PendingStarts};
use crate::net::NetConfig;
use crate::simulation::faction::Lifestyle;

/// Default bind/connect port for the relaunched host process. Distinct
/// from `net::lan::LAN_PORT` (5001) so discovery + game traffic don't
/// share a socket.
const DEFAULT_GAME_PORT: u16 = 5000;

/// Buffered MainMenu state (player-name text field).
#[derive(Resource)]
pub struct MainMenuState {
    pub player_name: String,
}

impl Default for MainMenuState {
    fn default() -> Self {
        // Default to OS username when available so most players don't have
        // to type. Falls back to "Player" if the env lookup fails.
        let name = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "Player".into());
        Self { player_name: name }
    }
}

/// Boot-time auto-route: when the binary was launched with networking CLI
/// flags (`--listen`, `--connect`, …) the user did NOT come through the
/// MainMenu — skip straight to the matching state. Runs once at Startup.
///
/// - `NetMode::Local` (no CLI flags) → stay in `MainMenu`.
/// - `NetMode::ListenServer` / `Client` → go to `MultiplayerLobby`.
/// - `NetMode::DedicatedServer` → go to `MultiplayerLobby` (server-only;
///   no UI, but the lobby state runs the server-side state machine).
pub fn main_menu_boot_route_system(
    net_cfg: Res<NetConfig>,
    mut next_state: ResMut<NextState<GameState>>,
) {
    use crate::net::NetMode;
    match net_cfg.mode {
        NetMode::Local => {} // stay in MainMenu
        NetMode::ListenServer | NetMode::DedicatedServer | NetMode::Client => {
            next_state.set(GameState::MultiplayerLobby);
        }
    }
}

/// Re-launch the current binary with `--listen --bind 0.0.0.0:<port>
/// --player NAME`. Spawns a fresh process so Lightyear's
/// `ServerPlugins::new(config)` picks up listen-server transport
/// (it consumes the config at install time and there's no way to swap
/// it on a running App). Successful spawn exits the parent.
fn relaunch_as_host(player_name: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--listen")
        .arg("--bind")
        .arg(format!("0.0.0.0:{DEFAULT_GAME_PORT}"))
        .arg("--player")
        .arg(player_name);
    info!("Re-launching as host: {:?}", cmd);
    cmd.spawn()?;
    // Exit the menu process so only the relaunched host App stays alive.
    std::process::exit(0);
}

pub fn main_menu_system(
    mut contexts: EguiContexts,
    mut state: ResMut<MainMenuState>,
    mut next_state: ResMut<NextState<GameState>>,
    mut starts: ResMut<PendingStarts>,
) {
    let ctx = contexts.ctx_mut();

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            ui.heading(egui::RichText::new("CivGame").size(48.0));
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Dwarf-Fortress-style civilization simulation")
                    .italics()
                    .weak(),
            );
            ui.add_space(40.0);

            // Player name applies to every mode (used as the
            // PendingReconnect / faction-assignment key in multiplayer
            // and as the slot label in singleplayer).
            ui.horizontal(|ui| {
                ui.add_space(ui.available_width() * 0.5 - 130.0);
                ui.label("Name:");
                ui.add(
                    egui::TextEdit::singleline(&mut state.player_name)
                        .desired_width(200.0),
                );
            });
            if state.player_name.trim().is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 100, 100),
                    "Pick a name to continue.",
                );
            }
            ui.add_space(24.0);

            let name_ok = !state.player_name.trim().is_empty();

            ui.scope(|ui| {
                ui.set_enabled(name_ok);

                if ui
                    .add_sized([260.0, 44.0], egui::Button::new("Singleplayer"))
                    .clicked()
                {
                    // Seed PendingStarts with one local slot. The slot's
                    // megachunk is filled in by spawn_select when the
                    // player clicks the map.
                    *starts = PendingStarts::singleplayer(
                        state.player_name.trim().to_string(),
                        Lifestyle::Settled,
                    );
                    next_state.set(GameState::SpawnSelect);
                }

                ui.add_space(10.0);

                // Host/Join: re-launch the binary with the right CLI
                // flags. Lightyear 0.19 consumes its NetConfig at install
                // time and there's no supported way to swap transports
                // on a running App — re-launching is the smallest diff
                // and matches every other indie LAN game.
                if ui
                    .add_sized([260.0, 44.0], egui::Button::new("Host LAN Game"))
                    .clicked()
                {
                    *starts = PendingStarts::singleplayer(
                        state.player_name.trim().to_string(),
                        Lifestyle::Settled,
                    );
                    if let Err(err) = relaunch_as_host(state.player_name.trim()) {
                        warn!("Host re-launch failed: {err}");
                        // Fall back to in-process preview so the user can
                        // at least see the lobby UI.
                        next_state.set(GameState::MultiplayerLobby);
                    }
                }

                ui.add_space(10.0);

                if ui
                    .add_sized([260.0, 44.0], egui::Button::new("Join LAN Game"))
                    .clicked()
                {
                    *starts = PendingStarts::default();
                    // Join leaves the lobby state empty — Join role
                    // gets a CLI flag at re-launch; here we just dive
                    // into MultiplayerLobby in client mode so the
                    // browser surfaces hosts before the user picks one.
                    next_state.set(GameState::MultiplayerLobby);
                }
            });

            ui.add_space(40.0);
            ui.label(
                egui::RichText::new("LAN multiplayer · re-launch model (v1)")
                    .small()
                    .weak(),
            );
        });
    });
}
