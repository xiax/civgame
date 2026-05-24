//! Lightyear protocol registration.
//!
//! `ProtocolPlugin` is installed on every Lightyear-bearing App (`Local`
//! co-located client+server, `ListenServer`, `DedicatedServer`, `Client`).
//! It registers the channels and wire messages used by Phase 2 so that
//! both sides agree on the on-the-wire shape.
//!
//! Channels:
//! - `OrderedReliableChannel` — commands, acks, hello, snapshot, tile
//!   diffs. All control-plane payloads ride here.
//!
//! Messages (with direction):
//! - `ClientHello` — Client → Server
//! - `FactionAssignment` — Server → Client
//! - `BootstrapSnapshot` — Server → Client
//! - `ChunkOverlayDelta` — Server → Client
//! - `NetCommandFrame` — Client → Server
//! - `NetCommandAck` — Server → Client
//!
//! Per Lightyear's docs (`register_message`) the protocol plugin must be
//! added *after* the Server/Client plugins so the registry can inspect
//! `ServerConfig` / `ClientConfig` and route the registration to the
//! correct side.

use bevy::prelude::*;
use lightyear::prelude::*;

use crate::net::protocol::{
    BootstrapSnapshot, ChunkOverlayDelta, ClientCameraFocus, ClientHello, EntityRemoved,
    EntityStateDelta, FactionAssignment, NetCommandAck, NetCommandFrame,
};

/// The single reliable channel every Phase 2 control message rides.
/// Phase 3 may add an `UnreliableSequenced` channel for per-tick transform
/// updates; for now everything is reliable.
#[derive(lightyear::prelude::Channel)]
pub struct OrderedReliableChannel;

pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.add_channel::<OrderedReliableChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        });

        // Client-issued.
        app.register_message::<ClientHello>(ChannelDirection::ClientToServer);
        app.register_message::<ClientCameraFocus>(ChannelDirection::ClientToServer);
        app.register_message::<NetCommandFrame>(ChannelDirection::ClientToServer);

        // Server-issued.
        app.register_message::<FactionAssignment>(ChannelDirection::ServerToClient);
        app.register_message::<BootstrapSnapshot>(ChannelDirection::ServerToClient);
        app.register_message::<ChunkOverlayDelta>(ChannelDirection::ServerToClient);
        app.register_message::<EntityStateDelta>(ChannelDirection::ServerToClient);
        app.register_message::<EntityRemoved>(ChannelDirection::ServerToClient);
        app.register_message::<NetCommandAck>(ChannelDirection::ServerToClient);
    }
}
