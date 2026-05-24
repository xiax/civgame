//! Wire-shaped types crossing the client↔server boundary.
//!
//! Phase 1 keeps the event in-process (the loopback validator immediately
//! re-emits as `PlayerCommandEvent`), so the inner `PlayerCommand` doesn't
//! need a serde derive yet. Phase 2 will add `Serialize` / `Deserialize` to
//! `PlayerCommand` (and its non-trivial sub-types — `VehicleGrid`, etc.) so
//! the same event can be put on Lightyear's reliable channel.
//!
//! The presence of this event type today is the load-bearing one-path
//! property: even in single-player every UI command crosses the network
//! boundary, so the server-auth path never atrophies.

use bevy::prelude::*;

use crate::simulation::player_command::PlayerCommand;

/// One UI- (or remote-client-)issued command, scoped to the faction that
/// claims to be sending it. The loopback validates `sender_faction_id`
/// against `ControlledFactions` before producing the `PlayerCommandEvent`.
///
/// `actors` carries entity targets the same way `PlayerCommandEvent` does;
/// in `DedicatedServer` mode (Phase 2) clients will refer to actors by
/// `NetId` instead and the server will resolve. For Phase 1 the in-process
/// client and server share the same World so the raw `Entity` is fine.
#[derive(Event, Debug, Clone)]
pub struct NetPlayerCommandEvent {
    pub sender_faction_id: u32,
    pub actors: Vec<Entity>,
    pub command: PlayerCommand,
}

/// Selects which net role this process plays. Read once at startup from
/// the CLI; resource value is stable for the rest of the session.
///
/// Phase 1 always runs `Local`. Phase 2 wires `ListenServer` /
/// `DedicatedServer` / `Client` to swap transports.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// Single-process, in-memory loopback. Same App runs the sim and the
    /// UI; the network boundary is the `NetPlayerCommandEvent` channel,
    /// not a socket.
    Local,
    /// Listen mode: `Local` + a UDP/QUIC socket accepting remote clients.
    /// Phase 2.
    ListenServer,
    /// Headless server: no `RenderingPlugin` / `UiPlugin`. Phase 2.
    DedicatedServer,
    /// Connect-only: no sim, only render/UI + replication. Phase 2.
    Client,
}

impl Default for NetMode {
    fn default() -> Self {
        NetMode::Local
    }
}
