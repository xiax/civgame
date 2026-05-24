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

pub mod protocol;
pub mod snapshot;

pub use protocol::{NetMode, NetPlayerCommandEvent};

/// Drains `NetPlayerCommandEvent`s, validates the declared sender faction
/// against `ControlledFactions`, and re-emits the inner command as a
/// `PlayerCommandEvent` for the sim's existing drain. This is the
/// network-boundary system: in `Local` mode it runs in-process; under
/// `DedicatedServer` mode (Phase 2) it runs after Lightyear's receive
/// step has produced the same event.
pub fn command_loopback_system(
    mut net_reader: EventReader<NetPlayerCommandEvent>,
    mut out: EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
    controlled: Res<crate::simulation::faction::ControlledFactions>,
) {
    for ev in net_reader.read() {
        if !controlled.contains(ev.sender_faction_id) {
            warn!(
                "drop NetPlayerCommand from faction {} (not controlled here: {:?})",
                ev.sender_faction_id, controlled.ids
            );
            continue;
        }
        out.send(crate::simulation::player_command::PlayerCommandEvent {
            actors: ev.actors.clone(),
            command: ev.command.clone(),
        });
    }
}

/// Install the network-boundary plumbing. Phase 1 wires only the loopback;
/// Phase 2 swaps the channel for Lightyear's `LocalChannel` / UDP transport
/// behind the same event API.
pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetMode>()
            .add_event::<NetPlayerCommandEvent>()
            // Loopback runs in `PreUpdate` so the sim's `Input`
            // (`drain_player_command_events_system`) sees fresh
            // `PlayerCommandEvent`s the same FixedUpdate tick.
            .add_systems(PreUpdate, command_loopback_system);
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
}
