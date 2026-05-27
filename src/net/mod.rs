//! Network-boundary plumbing for server-authoritative play.
//!
//! Phase 1 (this module today) — **pipeline unification**: every UI-issued
//! command flows UI → `CommandSender::send` → `NetPlayerCommandEvent` →
//! `command_loopback_system` → `PlayerCommandEvent` even in single-player.
//! The loopback validates ownership against `ControlledFactions` and is the
//! single place a "from the wire" command becomes a sim event. In `Local`
//! mode the transport hop is elided (in-process channel); Phase 2 will wrap
//! the channel in Lightyear so the same path carries remote commands.
//!
//! See `plans/multiplayer.md` for the full design.

use bevy::prelude::*;
use lightyear::prelude::client::{
    ClientCommandsExt, ClientConfig, ClientTransport, IoConfig as ClientIoConfig,
};
use lightyear::prelude::server::{
    NetConfig as ServerNetConfig, NetcodeConfig, ServerCommandsExt, ServerConfig, ServerTransport,
    IoConfig as ServerIoConfig,
};
use lightyear::prelude::{
    client::{Authentication, NetConfig as ClientNetConfig},
    SharedConfig, TickConfig,
};
use std::net::SocketAddr;
use std::time::Duration;

pub mod bootstrap;
pub mod cli;
pub mod client;
pub mod lan;
pub mod lobby_state;
pub mod protocol;
pub mod protocol_plugin;
pub mod server;
pub mod snapshot;

#[cfg(test)]
mod integration_tests;

#[allow(unused_imports)]
pub use cli::{parse_from_env, CliError, NetConfig};
pub use protocol::{NetMode, NetPlayerCommandEvent};

/// Run-condition predicate: true on every mode except `Client`. Wraps
/// the mutating sim/economy/pathfinding system sets so the client App
/// can co-exist with replication-driven state without locally
/// authoring the world. `NetMode::runs_sim()` mirrors this for
/// non-system call sites.
///
/// Returns `true` when `NetMode` isn't installed (headless test
/// fixtures, sandbox harnesses) so existing tests that never touch
/// `NetPlugin` keep their sim running.
pub fn net_mode_runs_sim(mode: Option<Res<NetMode>>) -> bool {
    match mode.as_deref() {
        Some(NetMode::Client) => false,
        _ => true,
    }
}

/// Stable protocol id for the Lightyear netcode handshake. Bump if the
/// on-the-wire protocol changes in a way that requires hard rejection
/// of older clients (today: keep in lockstep with
/// `protocol::PROTOCOL_VERSION`).
pub const NETCODE_PROTOCOL_ID: u64 = protocol::PROTOCOL_VERSION as u64;

/// Lightyear sim-tick cadence — must match the FixedUpdate hz in `main.rs`.
const NET_TICK_INTERVAL: Duration = Duration::from_millis(50); // 20 Hz

/// Client id used by the co-located host-server's own client. External
/// clients get netcode ids derived from `--player NAME` via
/// `derive_client_id`; the host's `Local` transport never touches the
/// netcode handshake so reusing `1` here can't collide with a derived
/// remote id.
const HOST_SERVER_LOCAL_CLIENT_ID: u64 = 1;

/// Shared dev-only netcode private key. Both server `NetcodeConfig` and
/// client `Authentication::Manual` must use the same key — the netcode
/// runtime verifies connect tokens against this key on every incoming
/// handshake. Phase 3f swap-target: production deployments should mint
/// per-client `ConnectToken`s from an out-of-band auth server keyed on
/// the real per-deployment private key. For LAN / dev / playtests this
/// fixed key + per-player derived client_id is the foothold.
pub const DEV_NETCODE_KEY: [u8; 32] = [0; 32];

/// Stable u64 derived from a player name. Two clients connecting with
/// the same `--player NAME` would collide on netcode `client_id`,
/// which the netcode runtime rejects as `AlreadyConnected` — that's
/// the desired behaviour (one human, one slot) and means players who
/// want to share a household just pick different names. Picks a wide
/// non-zero band by mixing a constant prefix so the hash can't collide
/// with the reserved `HOST_SERVER_LOCAL_CLIENT_ID` for non-empty names.
pub fn derive_client_id(player_name: &str) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    // ahash with a fixed key — `ahash::AHasher::default()` would key
    // off process-local randomness and produce a different id on each
    // restart, breaking reconnect-by-name. `RandomState::with_seeds`
    // pins the key so the derivation is process-independent.
    let state = ahash::RandomState::with_seeds(
        0x4356_4944_5f4e_4554, // "CVID_NET"
        0x4d50_5f43_4c49_4400, // "MP_CLID\0"
        0xa55a_a55a_a55a_a55a,
        0x5aa5_5aa5_5aa5_5aa5,
    );
    let mut hasher = state.build_hasher();
    "civgame-player-v1".hash(&mut hasher);
    player_name.hash(&mut hasher);
    let raw = hasher.finish();
    // Reserve client_ids 0 and HOST_SERVER_LOCAL_CLIENT_ID for the
    // local-transport host. Empty/whitespace name → bump into the
    // non-host band so a misconfigured client doesn't impersonate
    // the host slot.
    if raw == 0 || raw == HOST_SERVER_LOCAL_CLIENT_ID {
        return raw.wrapping_add(2);
    }
    raw
}

/// Drains `NetPlayerCommandEvent`s, validates the declared sender faction
/// against `ControlledFactions`, resolves wire-side `NetId` actors back to
/// live `Entity`s via `NetIdMap`, and re-emits the inner command as a
/// `PlayerCommandEvent` for the sim's existing drain. This is the
/// network-boundary system: in `Local` mode it runs in-process; under
/// `DedicatedServer` mode (Phase 2) it runs after Lightyear's receive
/// step has produced the same event.
///
/// Unresolvable `NetId`s (actor despawned mid-flight) are dropped silently
/// from the actor list. An event whose every actor was dropped is still
/// emitted — faction-level commands legitimately ship with an empty
/// `actors` list, and the drain validates `actors.is_empty()` semantics
/// per-variant.
pub fn command_loopback_system(
    mut net_reader: EventReader<NetPlayerCommandEvent>,
    mut out: EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
    net_ids: Res<crate::net_id::NetIdMap>,
) {
    for ev in net_reader.read() {
        if !controlled.contains(ev.sender_faction_id) {
            warn!(
                "drop NetPlayerCommand from faction {} (not controlled here: {:?})",
                ev.sender_faction_id, controlled.ids
            );
            continue;
        }
        let actors: Vec<bevy::prelude::Entity> = ev
            .actors
            .iter()
            .filter_map(|id| net_ids.entity_of(*id))
            .collect();
        out.send(crate::simulation::player_command::PlayerCommandEvent {
            actors,
            command: ev.command.clone(),
        });
    }
}

/// Install the network-boundary plumbing.
///
/// In `Local` mode this is just the in-process loopback and
/// `ConnectedRemotes` tracker — no Lightyear runtime.
///
/// In `ListenServer` / `DedicatedServer` mode we install
/// `ServerPlugins` + `ProtocolPlugin` + server systems
/// (`accept_connections_system`, `replicate_tile_overlays_system`,
/// `receive_command_frames_system`). `ListenServer` *also* installs
/// `ClientPlugins` with `NetConfig::Local` so the host plays through the
/// same path as a remote — no two-codepath drift.
///
/// In `Client` mode we install `ClientPlugins` + `ProtocolPlugin` +
/// client systems (`send_client_hello_system`,
/// `apply_bootstrap_snapshot_system`, `send_command_frames_system`).
/// No server plugins, no sim plugins.
pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        // NetMode default; main.rs pre-inserts the CLI choice so this
        // never overwrites.
        app.init_resource::<NetMode>()
            .add_event::<NetPlayerCommandEvent>()
            .add_event::<server::LocalLobbyCommand>()
            .init_resource::<DisconnectPolicy>()
            .init_resource::<ConnectedRemotes>()
            // Loopback runs in `PreUpdate` so the sim's `Input`
            // (`drain_player_command_events_system`) sees fresh
            // `PlayerCommandEvent`s the same FixedUpdate tick. Gated
            // off in `Client` mode — the client never *runs* the sim
            // locally, so re-emitting commands as `PlayerCommandEvent`
            // would only thrash state the server hasn't authorized yet.
            // Client's own UI command path will ride
            // `net::client::send_command_frames_system` instead.
            .add_systems(
                PreUpdate,
                command_loopback_system.run_if(|mode: Res<NetMode>| {
                    !matches!(*mode, NetMode::Client)
                }),
            );

        let mode = app
            .world()
            .get_resource::<NetMode>()
            .copied()
            .unwrap_or_default();

        // LAN browser: install in any mode that has UI. Cheap (one
        // listener thread that recv-times-out every 500ms), so the
        // singleplayer MainMenu can preview discovered hosts before
        // committing to Host/Join. Dedicated headless server doesn't
        // need a browser.
        if !matches!(mode, NetMode::DedicatedServer) {
            let browser = lan::spawn_listener_thread();
            app.insert_resource(browser);
        }

        // Host-side LAN advertiser: bind a broadcasting UDP socket so
        // remote clients see this lobby in their browser. Drives one
        // `LanAdvert` per second from a per-tick run-condition system.
        if matches!(mode, NetMode::ListenServer | NetMode::DedicatedServer) {
            let net_cfg = app
                .world()
                .get_resource::<cli::NetConfig>()
                .cloned()
                .unwrap_or_default();
            let bind_port = net_cfg
                .bind_addr
                .map(|a| a.port())
                .unwrap_or(5000);
            let host_name = net_cfg.player_name.unwrap_or_else(|| "Host".into());
            if let Some(advertiser) = lan::LanAdvertiser::new(
                format!("{}'s Game", host_name),
                host_name,
                bind_port,
                /*world_seed*/ 0,
                /*max_players*/ 4,
            ) {
                app.insert_resource(advertiser);
                app.add_systems(Update, broadcast_lan_advert_system);
            }
        }

        match mode {
            NetMode::Local => {
                // No Lightyear runtime; loopback is the only path.
            }
            NetMode::ListenServer | NetMode::DedicatedServer => {
                install_server(app, mode);
                if matches!(mode, NetMode::ListenServer) {
                    install_host_client(app);
                }
                install_protocol(app);
                install_server_systems(app);
                if matches!(mode, NetMode::ListenServer) {
                    install_client_systems(app);
                }
                app.add_systems(Startup, start_server_on_startup_system);
            }
            NetMode::Client => {
                install_client(app);
                install_protocol(app);
                install_client_systems(app);
                app.add_systems(Startup, connect_client_on_startup_system);
            }
        }

        // Disconnect policy + speed lock run in every mode — they're no-ops
        // when no remote ever connects.
        app.add_systems(Update, (apply_disconnect_policy_system, speed_lock_system));
    }
}

/// Tick the host-side LAN advertiser at most once per
/// `lan::BROADCAST_INTERVAL`. Runs in every host App regardless of
/// game state — the lobby browser is supposed to find the host even
/// before its first client picks a start.
pub fn broadcast_lan_advert_system(
    mut advertiser: ResMut<lan::LanAdvertiser>,
    state: Res<State<crate::GameState>>,
    server_conns: Option<Res<server::ServerConnections>>,
) {
    let phase = if matches!(state.get(), crate::GameState::Playing) {
        lan::AdvertPhase::InGame
    } else {
        lan::AdvertPhase::Lobby
    };
    let players = server_conns
        .as_ref()
        .map(|c| c.by_client.len() as u8)
        .unwrap_or(0);
    advertiser.maybe_broadcast(players, phase);
}

fn install_server(app: &mut App, mode: NetMode) {
    let net_cfg = app
        .world()
        .get_resource::<cli::NetConfig>()
        .cloned()
        .unwrap_or_default();
    let bind_addr: SocketAddr = net_cfg
        .bind_addr
        .unwrap_or_else(|| "0.0.0.0:5000".parse().expect("hardcoded socket parses"));

    // Shared dev key (`DEV_NETCODE_KEY`) instead of a per-startup
    // `generate_key()` — clients can only mint matching connect tokens
    // when both ends agree on the private key. Production should swap
    // to per-deployment keys + a real auth server (see Phase 3f notes).
    let netcode = NetcodeConfig::default()
        .with_protocol_id(NETCODE_PROTOCOL_ID)
        .with_key(DEV_NETCODE_KEY);
    let server_io = ServerIoConfig::from_transport(ServerTransport::UdpSocket(bind_addr));
    let net_config = ServerNetConfig::Netcode {
        config: netcode,
        io: server_io,
    };

    let shared = SharedConfig {
        tick: TickConfig::new(NET_TICK_INTERVAL),
        ..default()
    };

    let server_config = ServerConfig {
        shared,
        net: vec![net_config],
        ..default()
    };
    app.add_plugins(lightyear::prelude::server::ServerPlugins::new(server_config));

    info!(
        "Lightyear server installed (mode={:?}, bind={})",
        mode, bind_addr
    );
}

/// In `ListenServer` mode the host *also* plays. Install a `ClientPlugins`
/// alongside the server with `NetConfig::Local` (no socket — the host's
/// commands flow through Lightyear's in-process path).
fn install_host_client(app: &mut App) {
    let shared = SharedConfig {
        tick: TickConfig::new(NET_TICK_INTERVAL),
        ..default()
    };
    let client_config = ClientConfig {
        shared,
        net: ClientNetConfig::Local {
            id: HOST_SERVER_LOCAL_CLIENT_ID,
        },
        ..default()
    };
    app.add_plugins(lightyear::prelude::client::ClientPlugins::new(client_config));
    info!("Lightyear host-client installed (NetConfig::Local)");
}

fn install_client(app: &mut App) {
    let net_cfg = app
        .world()
        .get_resource::<cli::NetConfig>()
        .cloned()
        .unwrap_or_default();
    let server_addr = net_cfg
        .connect_addr
        .unwrap_or_else(|| "127.0.0.1:5000".parse().expect("hardcoded socket parses"));
    let client_addr: SocketAddr = "0.0.0.0:0"
        .parse()
        .expect("zero-port bind always parses");

    // Phase 3f: per-client netcode `client_id` derived deterministically
    // from `--player NAME` (`derive_client_id`). Same shared key as the
    // server (`DEV_NETCODE_KEY`) — production should mint per-client
    // `ConnectToken`s out-of-band keyed on a real private key, but for
    // LAN / dev / playtests the manual handshake gives every player a
    // unique slot (two clients sharing a name collide on `client_id`
    // and the second hits `AlreadyConnected`, which is correct).
    let player_name = net_cfg
        .player_name
        .clone()
        .unwrap_or_else(|| "Player".into());
    let client_id = derive_client_id(&player_name);
    let auth = Authentication::Manual {
        server_addr,
        protocol_id: NETCODE_PROTOCOL_ID,
        private_key: DEV_NETCODE_KEY,
        client_id,
    };

    let io = ClientIoConfig::from_transport(ClientTransport::UdpSocket(client_addr));

    let shared = SharedConfig {
        tick: TickConfig::new(NET_TICK_INTERVAL),
        ..default()
    };
    let client_config = ClientConfig {
        shared,
        net: ClientNetConfig::Netcode {
            auth,
            config: default(),
            io,
        },
        ..default()
    };
    app.add_plugins(lightyear::prelude::client::ClientPlugins::new(client_config));
    info!(
        "Lightyear client installed (connect to {}, player={:?}, client_id={})",
        server_addr, player_name, client_id
    );
}

fn install_protocol(app: &mut App) {
    app.add_plugins(protocol_plugin::ProtocolPlugin);
}

fn install_server_systems(app: &mut App) {
    app.init_resource::<server::ServerConnections>()
        .init_resource::<server::PendingDisconnects>()
        .init_resource::<server::ReplicationStats>()
        .init_resource::<server::LastKnownChunkMap>()
        .init_resource::<server::PendingReconnect>()
        .init_resource::<lobby_state::LobbyState>()
        .add_systems(
            Update,
            (
                server::accept_connections_system,
                server::handle_client_hello_system,
                server::record_disconnections_system,
                server::expire_pending_reconnects_system,
                server::receive_command_frames_system,
                server::receive_camera_focus_system,
                server::compute_interest_system,
                server::replicate_tile_overlays_system,
                server::replicate_entity_state_system,
                server::replicate_entity_removals_system,
                server::report_replication_stats_system,
                server::emit_tile_changed_for_replicated_entities_system,
            ),
        )
        .add_systems(
            Update,
            (
                server::handle_lobby_join_system,
                server::handle_lobby_select_start_system,
                server::handle_lobby_set_ready_system,
                server::handle_lobby_leave_system,
                server::broadcast_lobby_snapshot_system
                    .after(server::handle_lobby_join_system)
                    .after(server::handle_lobby_select_start_system)
                    .after(server::handle_lobby_set_ready_system)
                    .after(server::handle_lobby_leave_system),
                server::start_game_transition_system
                    .after(server::broadcast_lobby_snapshot_system),
            ),
        );
}

fn install_client_systems(app: &mut App) {
    app.init_resource::<client::ClientCommandSequencer>()
        .init_resource::<client::ClientAckLog>()
        .init_resource::<client::ReplicatedPlantMap>()
        .init_resource::<client::ReplicatedStructureMap>()
        .add_systems(
            Update,
            (
                client::send_client_hello_system,
                client::apply_bootstrap_snapshot_system,
                client::apply_entity_state_delta_system,
                client::apply_entity_removed_system,
                client::send_camera_focus_system,
                client::send_command_frames_system,
                client::observe_disconnect_system,
            ),
        )
        .add_systems(
            Update,
            (
                client::apply_lobby_snapshot_system,
                client::apply_lobby_start_game_system,
                client::apply_lobby_reject_system,
            ),
        );
}

fn start_server_on_startup_system(mut commands: Commands) {
    commands.start_server();
}

fn connect_client_on_startup_system(mut commands: Commands) {
    commands.connect_client();
}

// ============================================================================
// Disconnect policy + speed lock
// ============================================================================

/// What the server does when a client driving a faction disconnects.
/// Settable via CLI `--on-disconnect=...`; defaults to `AiTakeover`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisconnectPolicy {
    /// Strip the faction from `ControlledFactions` so the chief AI
    /// reclaims agency. Faction state persists; chief resumes posting.
    #[default]
    AiTakeover,
    /// Pause the simulation (sets virtual time scale to 0) until next
    /// reconnect. Single-faction servers should prefer this.
    Pause,
    /// Treat the faction as fallen — chief is despawned, faction is
    /// marked `materialized = false`. Phase 3 will model this properly;
    /// for now we just strip control.
    DropFaction,
}

impl DisconnectPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ai-takeover" => Some(Self::AiTakeover),
            "pause" => Some(Self::Pause),
            "drop-faction" => Some(Self::DropFaction),
            _ => None,
        }
    }
}

/// Drain `server::PendingDisconnects` (only populated when server systems
/// are installed) and apply the configured policy. Loopback-friendly: in
/// `Local` mode the resource exists but stays empty.
pub fn apply_disconnect_policy_system(
    mut pending: Option<ResMut<server::PendingDisconnects>>,
    mut controlled: ResMut<crate::simulation::faction::ControlledFactions>,
    policy: Res<DisconnectPolicy>,
    mut virtual_time: Option<ResMut<Time<Virtual>>>,
    mut remotes: ResMut<ConnectedRemotes>,
) {
    let Some(pending) = pending.as_mut() else {
        return;
    };
    if pending.0.is_empty() {
        return;
    }
    for faction_id in pending.0.drain(..) {
        // Symmetric with the server-side connect increment: every
        // disconnect drops the live-remote count so `speed_lock_system`
        // releases the 1× lock once the last remote leaves. Saturating
        // because a stray duplicate disconnect can't underflow.
        remotes.count = remotes.count.saturating_sub(1);
        match *policy {
            DisconnectPolicy::AiTakeover | DisconnectPolicy::DropFaction => {
                controlled.remove(faction_id);
                info!(
                    "policy {:?}: faction {} removed from ControlledFactions",
                    *policy, faction_id
                );
            }
            DisconnectPolicy::Pause => {
                if let Some(vt) = virtual_time.as_mut() {
                    vt.pause();
                }
                info!(
                    "policy Pause: sim paused after faction {} disconnect",
                    faction_id
                );
            }
        }
    }
}

/// Tracks how many remote clients are connected (excludes the host's
/// own NetConfig::Local connection). Updated by the server when accept /
/// disconnect events fire.
#[derive(Resource, Default, Debug)]
pub struct ConnectedRemotes {
    pub count: u32,
}

/// When ≥1 remote client connected, lock virtual time to 1× — pause
/// and speed presets are disabled in MP per `plans/multiplayer.md` 2d.
pub fn speed_lock_system(
    remotes: Res<ConnectedRemotes>,
    virtual_time: Option<ResMut<Time<Virtual>>>,
) {
    if remotes.count == 0 {
        return;
    }
    let Some(mut virtual_time) = virtual_time else {
        return;
    };
    let speed = virtual_time.relative_speed();
    if (speed - 1.0).abs() > f32::EPSILON {
        virtual_time.set_relative_speed(1.0);
    }
    if virtual_time.is_paused() {
        virtual_time.unpause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_id::NetIdPlugin;
    use crate::simulation::faction::ControlledFactions;
    use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};

    fn make_app() -> App {
        let mut app = App::new();
        app.add_plugins((NetIdPlugin, NetPlugin));
        app.add_event::<PlayerCommandEvent>();
        app.insert_resource(ControlledFactions::single(7));
        app
    }

    #[test]
    fn loopback_passes_controlled_faction_through() {
        let mut app = make_app();
        app.world_mut().send_event(NetPlayerCommandEvent {
            sender_faction_id: 7,
            actors: Vec::new(),
            command: PlayerCommand::EncodeTablet {
                tech: 0,
                faction_id: 7,
            },
        });
        app.update();
        // Drain `PlayerCommandEvent` to verify exactly one came through.
        let mut events = app
            .world_mut()
            .resource_mut::<Events<PlayerCommandEvent>>();
        let drained: Vec<_> = events.drain().collect();
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn loopback_drops_uncontrolled_faction() {
        let mut app = make_app();
        app.world_mut().send_event(NetPlayerCommandEvent {
            sender_faction_id: 99,
            actors: Vec::new(),
            command: PlayerCommand::EncodeTablet {
                tech: 0,
                faction_id: 99,
            },
        });
        app.update();
        let mut events = app
            .world_mut()
            .resource_mut::<Events<PlayerCommandEvent>>();
        let drained: Vec<_> = events.drain().collect();
        assert!(drained.is_empty(), "uncontrolled-faction command must drop");
    }

    #[test]
    fn derive_client_id_is_stable_across_calls() {
        assert_eq!(
            super::derive_client_id("Alice"),
            super::derive_client_id("Alice"),
        );
        assert_eq!(
            super::derive_client_id("Bob"),
            super::derive_client_id("Bob"),
        );
    }

    #[test]
    fn derive_client_id_distinguishes_names() {
        assert_ne!(
            super::derive_client_id("Alice"),
            super::derive_client_id("Bob"),
        );
    }

    #[test]
    fn derive_client_id_avoids_reserved_host_slot() {
        // Empty name still lands outside the reserved 0 / 1 slots so a
        // misconfigured client can't impersonate the host-local id.
        assert!(super::derive_client_id("") != 0);
        assert!(super::derive_client_id("") != super::HOST_SERVER_LOCAL_CLIENT_ID);
    }

    #[test]
    fn loopback_resolves_netid_actors_to_entities() {
        use crate::net_id::{NetId, NetIdMap, Networked};
        let mut app = make_app();
        let entity = app.world_mut().spawn(()).id();
        let net_id = {
            let mut map = app.world_mut().resource_mut::<NetIdMap>();
            map.alloc(entity)
        };
        app.world_mut().entity_mut(entity).insert(Networked(net_id));

        // Include a known-unresolvable NetId — should drop silently.
        let phantom = NetId(99_999);
        app.world_mut().send_event(NetPlayerCommandEvent {
            sender_faction_id: 7,
            actors: vec![net_id, phantom],
            command: PlayerCommand::Move {
                tile: (3, 4),
                z: 0,
            },
        });
        app.update();
        let mut events = app
            .world_mut()
            .resource_mut::<Events<PlayerCommandEvent>>();
        let drained: Vec<_> = events.drain().collect();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].actors, vec![entity]);
    }
}
