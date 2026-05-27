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
use crate::net::protocol::{
    LobbyLeave, LobbySelectStart, LobbySetReady, LobbySlotPublic,
};
use crate::net::protocol_plugin::OrderedReliableChannel;
use crate::net::server::LocalLobbyCommand;
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
    /// Mirror of the server's `LobbySnapshot.slots` for clients. The host
    /// renders directly off `LobbyState`; clients can't see that resource
    /// and so render from this buffer instead. Empty until the first
    /// `LobbySnapshot` arrives.
    pub remote_slots: Vec<LobbySlotPublic>,
    /// Local player's chosen megachunk (the one the client just sent or
    /// is about to send via `LobbySelectStart`). Pre-populated from
    /// `MultiplayerLobby` entry so even Hosting alone can pick a start.
    pub local_megachunk: Option<(i32, i32)>,
    /// Local Ready flag (mirrors the slot's ready state on the server).
    pub local_ready: bool,
}

/// Bundle the lobby-command emission channels — local host injects via
/// `LocalLobbyCommand` events; remote clients ship through Lightyear's
/// `ClientConnectionManager`. The lobby UI writes whichever channel
/// matches the local `NetMode`.
#[derive(bevy::ecs::system::SystemParam)]
pub struct LobbyCommandChannels<'w> {
    pub local: EventWriter<'w, LocalLobbyCommand>,
    pub client_mgr:
        Option<ResMut<'w, lightyear::prelude::client::ConnectionManager>>,
}

impl<'w> LobbyCommandChannels<'w> {
    /// `true` when the lobby command should be sent via wire (`Client`
    /// mode) rather than via `LocalLobbyCommand` (host).
    fn is_remote_client(mode: NetMode) -> bool {
        matches!(mode, NetMode::Client)
    }

    pub fn send_select_start(
        &mut self,
        mode: NetMode,
        client_id: u64,
        megachunk: (i32, i32),
    ) {
        if Self::is_remote_client(mode) {
            if let Some(mgr) = self.client_mgr.as_mut() {
                let _ = mgr.send_message::<OrderedReliableChannel, _>(&LobbySelectStart {
                    megachunk,
                });
            }
        } else {
            self.local.send(LocalLobbyCommand::SelectStart {
                client_id,
                megachunk,
            });
        }
    }

    pub fn send_set_ready(&mut self, mode: NetMode, client_id: u64, ready: bool) {
        if Self::is_remote_client(mode) {
            if let Some(mgr) = self.client_mgr.as_mut() {
                let _ = mgr.send_message::<OrderedReliableChannel, _>(&LobbySetReady { ready });
            }
        } else {
            self.local
                .send(LocalLobbyCommand::SetReady { client_id, ready });
        }
    }

    pub fn send_leave(&mut self, mode: NetMode, client_id: u64) {
        if Self::is_remote_client(mode) {
            if let Some(mgr) = self.client_mgr.as_mut() {
                let _ = mgr.send_message::<OrderedReliableChannel, _>(&LobbyLeave);
            }
        } else {
            self.local.send(LocalLobbyCommand::Leave { client_id });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn lobby_system(
    mut contexts: EguiContexts,
    net_cfg: Res<NetConfig>,
    mut ui_state: ResMut<LobbyUiState>,
    mut starts: ResMut<PendingStarts>,
    mut options: ResMut<GameStartOptions>,
    mut world_seed: ResMut<WorldSeed>,
    mut next_state: ResMut<NextState<GameState>>,
    lan_browser: Option<Res<LanBrowser>>,
    mut channels: LobbyCommandChannels,
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

    // Resolve the local client id. Host runs on the reserved local
    // transport id; remote clients derive deterministically from
    // `--player NAME`.
    let local_name = net_cfg
        .player_name
        .clone()
        .unwrap_or_else(|| "Player".into());
    let local_client_id = if matches!(net_cfg.mode, NetMode::Client) {
        crate::net::derive_client_id(&local_name)
    } else {
        HOST_SERVER_LOCAL_CLIENT_ID
    };

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Players");
        ui.add_space(6.0);

        // Client renders from the server-pushed `remote_slots` mirror;
        // host/local render from the live `PendingStarts.slots` (pre-
        // start) or `LobbyState` mirror (post-bootstrap). For the
        // pre-protocol flow we already had, `starts.slots` covers it.
        let render_remote = matches!(net_cfg.mode, NetMode::Client);
        let slot_count = if render_remote {
            ui_state.remote_slots.len()
        } else {
            starts.slots.len()
        };
        if slot_count == 0 {
            ui.label(egui::RichText::new("No players yet.").italics().weak());
        } else if render_remote {
            for slot in ui_state.remote_slots.iter() {
                ui.horizontal(|ui| {
                    let badge = if slot.is_local { " (you)" } else { "" };
                    ui.label(
                        egui::RichText::new(format!(
                            "[{}] {}{}",
                            slot.slot_id, slot.player_name, badge
                        ))
                        .strong(),
                    );
                    ui.separator();
                    if let Some((mx, my)) = slot.megachunk {
                        ui.label(format!("@ ({mx},{my})"));
                    } else {
                        ui.label(egui::RichText::new("(no start selected)").weak());
                    }
                    ui.separator();
                    if slot.ready {
                        ui.label(egui::RichText::new("Ready").color(egui::Color32::GREEN));
                    } else {
                        ui.label(egui::RichText::new("…").weak());
                    }
                });
            }
        } else {
            for slot in starts.slots.iter_mut() {
                ui.horizontal(|ui| {
                    let is_local = slot.client_id == HOST_SERVER_LOCAL_CLIENT_ID;
                    let badge = if is_local { " (you)" } else { "" };
                    ui.label(
                        egui::RichText::new(format!(
                            "[{}] {}{}",
                            slot.slot_id, slot.player_name, badge
                        ))
                        .strong(),
                    );
                    ui.separator();
                    if let Some((mx, my)) = slot.megachunk {
                        ui.label(format!("@ ({mx},{my})"));
                    } else {
                        ui.label(egui::RichText::new("(no start selected)").weak());
                    }
                    ui.separator();
                    let mut ready = slot.ready;
                    if ui.checkbox(&mut ready, "Ready").changed() {
                        slot.ready = ready;
                    }
                });
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        // Local-slot controls — pick a megachunk + toggle Ready. Ride
        // the lobby command channel so the server stays authoritative.
        ui.label(egui::RichText::new("Your start").strong());
        ui.horizontal(|ui| {
            ui.label("megachunk:");
            ui.add(
                egui::TextEdit::singleline(&mut ui_state.local_megachunk_text)
                    .desired_width(96.0)
                    .hint_text("x,y"),
            );
            if ui.button("Set").clicked() {
                if let Some(mega) = parse_megachunk(&ui_state.local_megachunk_text) {
                    ui_state.local_megachunk = Some(mega);
                    channels.send_select_start(net_cfg.mode, local_client_id, mega);
                }
            }
            if let Some((mx, my)) = ui_state.local_megachunk {
                ui.label(format!("(set to {mx},{my})"));
            }
        });
        ui.horizontal(|ui| {
            let mut ready = ui_state.local_ready;
            if ui.checkbox(&mut ready, "I'm ready").changed() {
                ui_state.local_ready = ready;
                channels.send_set_ready(net_cfg.mode, local_client_id, ready);
            }
        });

        ui.add_space(20.0);
        ui.separator();
        ui.add_space(10.0);

        if is_host {
            // Host's view of "all ready" reads from `PendingStarts.slots`
            // (host App also tracks LobbyState; the auto-bump promotes
            // to Starting → start_game_transition_system handles the
            // transition). Surface a hint while waiting.
            let all_ready = !starts.slots.is_empty()
                && starts
                    .slots
                    .iter()
                    .all(|s| s.ready && s.megachunk.is_some());
            ui.label(
                egui::RichText::new(if all_ready {
                    "All ready — starting…"
                } else {
                    "Every slot must pick a start and tick Ready."
                })
                .small()
                .weak(),
            );
            // Legacy quick-start for solo testing: lets a host bypass the
            // lobby entirely when no other clients connected.
            if starts.slots.len() <= 1 {
                if ui
                    .add_sized([220.0, 36.0], egui::Button::new("Solo Start"))
                    .clicked()
                {
                    if let Some(slot) = starts
                        .slots
                        .iter()
                        .find(|s| s.client_id == HOST_SERVER_LOCAL_CLIENT_ID)
                    {
                        starts.primary_start = slot.megachunk;
                    }
                    next_state.set(GameState::Playing);
                }
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

/// OnEnter(MultiplayerLobby): announce the local player to the lobby
/// state machine. Host injects via `LocalLobbyCommand::Join`; the remote
/// client's `ClientHello` already covers protocol announcement (server
/// turns it into a slot when it sees the matching `LobbyJoin`).
pub fn auto_join_lobby_on_enter(
    net_cfg: Res<NetConfig>,
    mut local: EventWriter<LocalLobbyCommand>,
    mut client_mgr: Option<ResMut<lightyear::prelude::client::ConnectionManager>>,
) {
    let player_name = net_cfg
        .player_name
        .clone()
        .unwrap_or_else(|| "Host".into());
    match net_cfg.mode {
        NetMode::Client => {
            if let Some(mgr) = client_mgr.as_mut() {
                let join = crate::net::protocol::LobbyJoin {
                    protocol_version: crate::net::protocol::PROTOCOL_VERSION,
                    player_name,
                };
                let _ = mgr.send_message::<OrderedReliableChannel, _>(&join);
            }
        }
        NetMode::Local | NetMode::ListenServer | NetMode::DedicatedServer => {
            local.send(LocalLobbyCommand::Join {
                player_name,
                client_id: HOST_SERVER_LOCAL_CLIENT_ID,
            });
        }
    }
}

fn parse_megachunk(s: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = s.split(|c: char| c == ',' || c.is_whitespace()).collect();
    let xs: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if xs.len() != 2 {
        return None;
    }
    let x = xs[0].parse::<i32>().ok()?;
    let y = xs[1].parse::<i32>().ok()?;
    Some((x, y))
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
