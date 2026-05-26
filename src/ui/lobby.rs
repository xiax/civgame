//! Multiplayer lobby UI (host + join roles). Renders in
//! `GameState::MultiplayerLobby`.
//!
//! v1 scaffold: shows the slot list, host config sub-panel, ready toggle,
//! and Start button. The protocol wiring (Phase 3) + LAN browser (Phase 5)
//! land in subsequent commits — this file is the in-tree entry point so
//! the state machine is exercised end-to-end.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::game_state::{
    EconomyPreset, GameStartOptions, GameState, PendingStarts, StartSettlementMaturity, WorldSeed,
    HOST_SERVER_LOCAL_CLIENT_ID,
};
use crate::net::lan::LanBrowser;
use crate::net::{NetConfig, NetMode};
use crate::simulation::faction::Lifestyle;
use crate::simulation::technology::Era;

/// UI-only buffered state (seed text, address fields). Wire-state lives
/// server-side in `LobbyState` / client-side in network appliers.
#[derive(Resource, Default)]
pub struct LobbyUiState {
    /// Address text field used by Join role. Tries `127.0.0.1:5000` by
    /// default so single-machine LAN testing works without typing.
    pub manual_join_addr: String,
    /// Buffered seed text (matches spawn_select's UX so reroll feels the
    /// same in both flows).
    pub seed_text: String,
    /// Selected megachunk for the local player's slot. Lobby browser sets
    /// this via map click; locally mirrored onto `PendingStarts.slots[i]`.
    pub local_megachunk_text: String,
}

pub fn lobby_system(
    mut contexts: EguiContexts,
    net_cfg: Res<NetConfig>,
    mut ui_state: ResMut<LobbyUiState>,
    mut starts: ResMut<PendingStarts>,
    mut options: ResMut<GameStartOptions>,
    mut world_seed: ResMut<WorldSeed>,
    mut next_state: ResMut<NextState<GameState>>,
    lan_browser: Option<Res<LanBrowser>>,
) {
    let ctx = contexts.ctx_mut();

    // Server role for the UI: ListenServer + Local both render the host
    // side; Client renders the join side. DedicatedServer typically has
    // no window — but if it does, render the host side.
    let is_host = matches!(
        net_cfg.mode,
        NetMode::Local | NetMode::ListenServer | NetMode::DedicatedServer
    );

    if ui_state.seed_text.is_empty() {
        ui_state.seed_text = world_seed.0.to_string();
    }

    egui::SidePanel::left("lobby_config")
        .resizable(false)
        .default_width(280.0)
        .show(ctx, |ui| {
            ui.add_space(10.0);
            ui.heading(if is_host { "Host Lobby" } else { "Join Lobby" });
            ui.add_space(8.0);

            if is_host {
                host_config_ui(ui, &mut ui_state, &mut options, &mut world_seed);
            } else {
                join_config_ui(ui, &mut ui_state, lan_browser.as_deref());
            }

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(10.0);
            if ui.button("← Back to Main Menu").clicked() {
                next_state.set(GameState::MainMenu);
            }
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Players");
        ui.add_space(6.0);

        if starts.slots.is_empty() {
            ui.label(egui::RichText::new("No players yet.").italics().weak());
        } else {
            for slot in starts.slots.iter_mut() {
                ui.horizontal(|ui| {
                    let is_local = slot.client_id == HOST_SERVER_LOCAL_CLIENT_ID;
                    let badge = if is_local { " (you)" } else { "" };
                    ui.label(egui::RichText::new(format!(
                        "[{}] {}{}",
                        slot.slot_id, slot.player_name, badge
                    )).strong());
                    ui.separator();
                    if let Some((mx, my)) = slot.megachunk {
                        ui.label(format!("@ ({mx},{my})"));
                    } else {
                        ui.label(egui::RichText::new("(no start selected)").weak());
                    }
                    ui.separator();
                    ui.checkbox(&mut slot.ready, "Ready");
                });
            }
        }

        ui.add_space(20.0);
        ui.separator();
        ui.add_space(10.0);

        if is_host {
            let all_ready = !starts.slots.is_empty()
                && starts.slots.iter().all(|s| s.ready && s.megachunk.is_some());
            ui.scope(|ui| {
                ui.set_enabled(all_ready);
                if ui
                    .add_sized([220.0, 36.0], egui::Button::new("Start Game"))
                    .clicked()
                {
                    // Promote local slot's megachunk to primary_start so
                    // the camera lands there on OnEnter(Playing).
                    if let Some(slot) = starts
                        .slots
                        .iter()
                        .find(|s| s.client_id == HOST_SERVER_LOCAL_CLIENT_ID)
                    {
                        starts.primary_start = slot.megachunk;
                    }
                    next_state.set(GameState::Playing);
                }
            });
            if !all_ready {
                ui.label(
                    egui::RichText::new(
                        "Every slot must pick a start and tick Ready.",
                    )
                    .small()
                    .weak(),
                );
            }
        } else {
            ui.label(
                egui::RichText::new("Waiting for host to start the game…")
                    .italics()
                    .weak(),
            );
        }
    });
}

fn host_config_ui(
    ui: &mut egui::Ui,
    ui_state: &mut LobbyUiState,
    options: &mut GameStartOptions,
    world_seed: &mut WorldSeed,
) {
    ui.label(egui::RichText::new("Era").strong());
    for era in [
        Era::Paleolithic,
        Era::Mesolithic,
        Era::Neolithic,
        Era::Chalcolithic,
        Era::BronzeAge,
    ] {
        ui.radio_value(&mut options.era, era, era.name());
    }
    ui.add_space(8.0);

    ui.label(egui::RichText::new("Economy").strong());
    ui.radio_value(&mut options.economy, EconomyPreset::Subsistence, "Subsistence");
    ui.radio_value(&mut options.economy, EconomyPreset::Mixed, "Mixed");
    ui.radio_value(&mut options.economy, EconomyPreset::Market, "Market");
    ui.add_space(8.0);

    ui.label(egui::RichText::new("Maturity").strong());
    ui.radio_value(&mut options.maturity, StartSettlementMaturity::Founder, "Founder");
    ui.radio_value(&mut options.maturity, StartSettlementMaturity::Established, "Established");
    ui.radio_value(&mut options.maturity, StartSettlementMaturity::Developed, "Developed");
    ui.add_space(8.0);

    ui.label(egui::RichText::new("World seed").strong());
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut ui_state.seed_text)
                .desired_width(120.0),
        );
        if ui.button("Apply").clicked() {
            if let Ok(v) = ui_state.seed_text.parse::<u64>() {
                world_seed.0 = v;
            }
        }
        if ui.button("Reroll").clicked() {
            world_seed.0 = fastrand::u64(..);
            ui_state.seed_text = world_seed.0.to_string();
        }
    });
}

fn join_config_ui(
    ui: &mut egui::Ui,
    ui_state: &mut LobbyUiState,
    lan_browser: Option<&LanBrowser>,
) {
    ui.label(egui::RichText::new("LAN browser").strong());
    if let Some(browser) = lan_browser {
        let live = browser.fresh();
        if live.is_empty() {
            ui.label(
                egui::RichText::new("(no hosts seen on this LAN — broadcasts every 1s)")
                    .small()
                    .weak(),
            );
        } else {
            for entry in live {
                let addr = format!("{}:{}", entry.host_addr.ip(), entry.advert.game_port);
                if ui
                    .selectable_label(
                        ui_state.manual_join_addr == addr,
                        format!(
                            "{} — {} ({}/{} players)",
                            entry.advert.game_name,
                            addr,
                            entry.advert.players,
                            entry.advert.max_players
                        ),
                    )
                    .clicked()
                {
                    ui_state.manual_join_addr = addr;
                }
            }
        }
    } else {
        ui.label(
            egui::RichText::new("(LAN listener not started in this mode)")
                .small()
                .weak(),
        );
    }

    ui.add_space(8.0);
    ui.label(egui::RichText::new("Server address").strong());
    if ui_state.manual_join_addr.is_empty() {
        ui_state.manual_join_addr = "127.0.0.1:5000".into();
    }
    ui.add(
        egui::TextEdit::singleline(&mut ui_state.manual_join_addr)
            .desired_width(180.0),
    );
    ui.label(
        egui::RichText::new("Manual entry overrides the browser selection.")
            .small()
            .weak(),
    );
    ui.add_space(8.0);

    let _ = Lifestyle::Settled;
}
