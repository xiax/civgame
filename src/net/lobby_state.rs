//! Server-authoritative lobby state machine.
//!
//! Sits server-side only (host App in `Local` / `ListenServer`, or the
//! `DedicatedServer` App). Clients read it indirectly via `LobbySnapshot`
//! broadcasts.
//!
//! Phase transitions:
//! ```text
//! Hosting → SelectingStarts → Starting → InGame
//! ```
//!
//! - `Hosting`: lobby created, accepting joins; host editing config.
//! - `SelectingStarts`: every joined client picking a start megachunk
//!   and toggling Ready. Phase entered automatically once the first
//!   client joins; left back to `Hosting` if every client leaves.
//! - `Starting`: host pressed Start; faction ids allocated, `LobbyStartGame`
//!   shipped to clients, server transitions into `Playing` next tick.
//! - `InGame`: post-start state; new joins rejected unless they match a
//!   `PendingReconnect` entry keyed on `player_name`.
//!
//! Validation predicates (`is_start_megachunk_acceptable`,
//! `lobby_ready_to_start`) live in `net::protocol` so they can be unit-
//! tested without an `App`.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::game_state::{EconomyPreset, StartSettlementMaturity};
use crate::net::protocol::LobbySlotPublic;
use crate::simulation::faction::Lifestyle;
use crate::simulation::technology::Era;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LobbyPhase {
    #[default]
    Hosting,
    SelectingStarts,
    Starting,
    InGame,
}

/// One server-side slot. Tracks the client's session-stable id, picked
/// megachunk, lifestyle choice, and ready flag. `faction_id` is `None`
/// until `LobbyStartGame` fires.
#[derive(Debug, Clone)]
pub struct ServerLobbySlot {
    pub slot_id: u8,
    pub player_name: String,
    pub client_id: u64,
    pub megachunk: Option<(i32, i32)>,
    pub lifestyle: Lifestyle,
    pub ready: bool,
    pub faction_id: Option<u32>,
}

/// Host-editable lobby configuration. Mirrors the subset of
/// `GameStartOptions` the lobby UI exposes plus `world_seed`.
#[derive(Debug, Clone)]
pub struct LobbyConfig {
    pub game_name: String,
    pub world_seed: u64,
    pub era: Era,
    pub economy: EconomyPreset,
    pub maturity: StartSettlementMaturity,
    pub max_players: u8,
}

impl Default for LobbyConfig {
    fn default() -> Self {
        Self {
            game_name: "CivGame Lobby".into(),
            world_seed: 42,
            era: Era::Neolithic,
            economy: EconomyPreset::Subsistence,
            maturity: StartSettlementMaturity::Established,
            max_players: 4,
        }
    }
}

/// Server-side authoritative lobby state. Installed only on hosts /
/// dedicated servers (clients carry their own UI buffer).
#[derive(Resource, Debug, Clone)]
pub struct LobbyState {
    pub phase: LobbyPhase,
    pub config: LobbyConfig,
    pub slots: Vec<ServerLobbySlot>,
    /// Bumped on every mutation so the server's snapshot broadcaster can
    /// dedup work — compare against a `Local<u32>` last-seen version.
    pub version: u32,
}

impl Default for LobbyState {
    fn default() -> Self {
        Self {
            phase: LobbyPhase::Hosting,
            config: LobbyConfig::default(),
            slots: Vec::new(),
            version: 0,
        }
    }
}

impl LobbyState {
    pub fn slot_for_client(&self, client_id: u64) -> Option<&ServerLobbySlot> {
        self.slots.iter().find(|s| s.client_id == client_id)
    }

    pub fn slot_for_client_mut(&mut self, client_id: u64) -> Option<&mut ServerLobbySlot> {
        self.slots.iter_mut().find(|s| s.client_id == client_id)
    }

    /// True iff `LobbyJoin` from this name should be accepted into the
    /// lobby right now. Reclaims an existing slot with the same name when
    /// possible. Otherwise capacity-limited by `max_players`.
    pub fn accepts_join(&self, player_name: &str) -> bool {
        if self.phase == LobbyPhase::InGame {
            return false;
        }
        if self.slots.iter().any(|s| s.player_name == player_name) {
            return true;
        }
        (self.slots.len() as u8) < self.config.max_players
    }

    /// Allocate the next slot id. Stable + monotonic per lobby session.
    pub fn next_slot_id(&self) -> u8 {
        let mut id = 0u8;
        loop {
            if !self.slots.iter().any(|s| s.slot_id == id) {
                return id;
            }
            id = id.saturating_add(1);
        }
    }

    /// Project the current slot list into wire form.
    pub fn public_snapshot(&self) -> Vec<LobbySlotPublic> {
        self.slots
            .iter()
            .map(|s| LobbySlotPublic {
                slot_id: s.slot_id,
                player_name: s.player_name.clone(),
                megachunk: s.megachunk,
                lifestyle_is_nomadic: matches!(s.lifestyle, Lifestyle::Nomadic),
                ready: s.ready,
                is_local: false,
            })
            .collect()
    }

    pub fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
        // Auto-advance phase based on slot occupancy + readiness.
        let all_ready_with_starts = !self.slots.is_empty()
            && self
                .slots
                .iter()
                .all(|s| s.ready && s.megachunk.is_some());
        self.phase = match self.phase {
            LobbyPhase::Hosting if !self.slots.is_empty() => LobbyPhase::SelectingStarts,
            LobbyPhase::SelectingStarts if self.slots.is_empty() => LobbyPhase::Hosting,
            LobbyPhase::SelectingStarts if all_ready_with_starts => LobbyPhase::Starting,
            other => other,
        };
    }

    /// True iff this candidate megachunk is far enough from every other
    /// slot's chosen megachunk to satisfy `MIN_HUMAN_MEGACHUNK_DISTANCE`.
    /// Pure — globe habitability is checked separately at the call site.
    pub fn is_select_acceptable(&self, candidate: (i32, i32), self_client_id: u64) -> bool {
        use crate::net::protocol::is_start_megachunk_acceptable;
        let others: Vec<(i32, i32)> = self
            .slots
            .iter()
            .filter(|s| s.client_id != self_client_id)
            .filter_map(|s| s.megachunk)
            .collect();
        is_start_megachunk_acceptable(candidate, &others)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_join_reclaims_existing_name_even_when_full() {
        let mut lobby = LobbyState::default();
        lobby.config.max_players = 1;
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "Alice".into(),
            client_id: 1,
            megachunk: None,
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
        assert!(lobby.accepts_join("Alice"), "name match overrides cap");
        assert!(!lobby.accepts_join("Bob"), "fresh name + full lobby = reject");
    }

    #[test]
    fn accepts_join_rejects_after_ingame() {
        let mut lobby = LobbyState::default();
        lobby.phase = LobbyPhase::InGame;
        assert!(!lobby.accepts_join("anyone"));
    }

    #[test]
    fn next_slot_id_fills_gaps() {
        let mut lobby = LobbyState::default();
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "a".into(),
            client_id: 1,
            megachunk: None,
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
        lobby.slots.push(ServerLobbySlot {
            slot_id: 2,
            player_name: "c".into(),
            client_id: 2,
            megachunk: None,
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
        assert_eq!(lobby.next_slot_id(), 1, "fills slot id gap");
    }

    #[test]
    fn bump_advances_to_starting_when_all_ready() {
        let mut lobby = LobbyState::default();
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "alice".into(),
            client_id: 1,
            megachunk: Some((5, 5)),
            lifestyle: Lifestyle::Settled,
            ready: true,
            faction_id: None,
        });
        lobby.bump();
        // First bump promotes Hosting → SelectingStarts.
        assert_eq!(lobby.phase, LobbyPhase::SelectingStarts);
        lobby.bump();
        // With all-ready-with-starts, second bump promotes to Starting.
        assert_eq!(lobby.phase, LobbyPhase::Starting);
    }

    #[test]
    fn bump_holds_in_selecting_starts_until_ready() {
        let mut lobby = LobbyState::default();
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "alice".into(),
            client_id: 1,
            megachunk: Some((5, 5)),
            lifestyle: Lifestyle::Settled,
            ready: false, // not ready
            faction_id: None,
        });
        lobby.bump();
        assert_eq!(lobby.phase, LobbyPhase::SelectingStarts);
        lobby.bump();
        assert_eq!(lobby.phase, LobbyPhase::SelectingStarts);
    }

    #[test]
    fn select_acceptable_rejects_too_close() {
        let mut lobby = LobbyState::default();
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "alice".into(),
            client_id: 1,
            megachunk: Some((0, 0)),
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
        // Same megachunk → reject.
        assert!(!lobby.is_select_acceptable((0, 0), 2));
        // 2 mega-chunks away → reject (< MIN_HUMAN_MEGACHUNK_DISTANCE=3).
        assert!(!lobby.is_select_acceptable((2, 0), 2));
        // 3 → accept.
        assert!(lobby.is_select_acceptable((3, 0), 2));
        // Picking the same chunk as your own existing pick → fine (self).
        assert!(lobby.is_select_acceptable((0, 0), 1));
    }

    #[test]
    fn bump_auto_advances_to_selecting_starts() {
        let mut lobby = LobbyState::default();
        assert_eq!(lobby.phase, LobbyPhase::Hosting);
        lobby.slots.push(ServerLobbySlot {
            slot_id: 0,
            player_name: "p".into(),
            client_id: 1,
            megachunk: None,
            lifestyle: Lifestyle::Settled,
            ready: false,
            faction_id: None,
        });
        lobby.bump();
        assert_eq!(lobby.phase, LobbyPhase::SelectingStarts);
        lobby.slots.clear();
        lobby.bump();
        assert_eq!(lobby.phase, LobbyPhase::Hosting);
    }
}
