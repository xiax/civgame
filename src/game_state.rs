//! Top-level lifecycle state for the game.
//!
//! - `MainMenu`: title screen with Singleplayer / Host LAN / Join LAN.
//! - `SpawnSelect`: world map shown; SP player picks a starting mega-chunk.
//! - `MultiplayerLobby`: LAN lobby (host or join role); resolves into
//!   `Playing` once `LobbyStartGame` fires.
//! - `Playing`: sim + chunk streaming active.

use bevy::prelude::*;

#[derive(States, Clone, Copy, Eq, PartialEq, Hash, Debug, Default)]
pub enum GameState {
    #[default]
    MainMenu,
    SpawnSelect,
    MultiplayerLobby,
    Playing,
}

/// Sub-state of `GameState::Playing` gating the unified settlement build
/// pipeline. `Warmup` covers the first ticks after `OnEnter(Playing)` while
/// the initial `SettlementBrain` survey + seed pass runs; per-tick agent
/// simulation is **not** gated on `Active` today (the agents still tick
/// during Warmup) but this state exists so future systems can opt in via
/// `.run_if(in_state(SimulationState::Active))` without further plumbing.
///
/// Flipped to `Active` by `mark_warmup_complete_system` once every settled
/// faction has had its initial survey applied and `seed_starting_buildings_system`
/// has run.
#[derive(SubStates, Clone, Copy, Eq, PartialEq, Hash, Debug, Default)]
#[source(GameState = GameState::Playing)]
pub enum SimulationState {
    #[default]
    Warmup,
    Active,
}

/// Pending spawn-cell choice from the spawn-select UI. Read once on
/// `OnEnter(Playing)` to position the camera and seed the player faction.
///
/// Compat view of `PendingStarts.primary_start`; mirrored every frame by
/// `legacy_pending_spawn_compat_system` (`game_state::plugin`). All new
/// readers should consume `PendingStarts` directly; legacy readers
/// (`world::spawn_world_system`, `simulation::person::spawn_population`'s
/// fallback, `rendering::camera::position_camera_for_spawn`, and a few
/// abstract-faction sites) still read this resource and will be migrated
/// alongside their respective subsystems.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PendingSpawn(pub Option<(i32, i32)>);

/// Reserved client-id used by the host's *local* App-side faction (i.e. the
/// listen-server host, or any singleplayer session). Matches
/// `net::HOST_SERVER_LOCAL_CLIENT_ID` so spawn-time logic can decide
/// "is this the local human?" without depending on the `net` module.
pub const HOST_SERVER_LOCAL_CLIENT_ID: u64 = 1;

/// Per-human-slot record describing one player-faction start, populated by
/// either the spawn-select UI (single slot, singleplayer) or the lobby
/// (one slot per joined client, multiplayer).
///
/// Read once at `OnEnter(GameState::Playing)` by `spawn_population` (one
/// human faction per slot) and by `spawn_world_system` (pre-gen the union
/// of windows around every `megachunk`).
#[derive(Debug, Clone)]
pub struct PlayerStartSlot {
    /// Stable position in `PendingStarts.slots` (0..). Used by the lobby UI
    /// and as a tie-breaker for deterministic faction-id assignment.
    pub slot_id: u8,
    /// Display name shown in the lobby + used as the `PendingReconnect`
    /// key so a returning client reclaims its slot/faction.
    pub player_name: String,
    /// Lightyear netcode client id (or `HOST_SERVER_LOCAL_CLIENT_ID` for
    /// the local-process host / singleplayer slot). `spawn_population`
    /// sets `PlayerFaction` for the slot whose id matches the local
    /// client.
    pub client_id: u64,
    /// Starting mega-chunk; `None` means the lobby has the slot but the
    /// player hasn't picked yet. `spawn_population` falls back to the
    /// globe-centre habitable cell if `None` survives to game start
    /// (matches the SP no-PendingSpawn fallback).
    pub megachunk: Option<(i32, i32)>,
    /// Faction archetype for this slot (Settled vs Nomadic). Per-slot so
    /// each player can pick independently in the lobby.
    pub lifestyle: crate::simulation::faction::Lifestyle,
    /// Lobby-ready flag (host requires every slot ready before Start).
    pub ready: bool,
    /// Assigned at the end of `spawn_population`. Surfaced back to the
    /// network layer via `FactionAssignment` for clients.
    pub faction_id: Option<u32>,
}

impl PlayerStartSlot {
    /// Singleplayer convenience: one slot for the local host with the
    /// supplied name + lifestyle.
    pub fn singleplayer(name: impl Into<String>, lifestyle: crate::simulation::faction::Lifestyle) -> Self {
        Self {
            slot_id: 0,
            player_name: name.into(),
            client_id: HOST_SERVER_LOCAL_CLIENT_ID,
            megachunk: None,
            lifestyle,
            ready: true,
            faction_id: None,
        }
    }
}

/// Multi-slot starting-cell selection. Always populated by the time
/// `OnEnter(GameState::Playing)` fires; singleplayer fills one slot,
/// multiplayer one slot per connected client. `primary_start` is the
/// camera anchor for the local client.
#[derive(Resource, Default, Debug, Clone)]
pub struct PendingStarts {
    /// Camera-anchor mega-chunk for the local client (camera scrolls here
    /// on `OnEnter(Playing)`; falls back to `slots[0].megachunk` if unset).
    pub primary_start: Option<(i32, i32)>,
    /// One entry per human start. Empty in pre-MainMenu boot state.
    pub slots: Vec<PlayerStartSlot>,
}

impl PendingStarts {
    /// Convenience: singleplayer init with one slot whose megachunk gets
    /// filled in by the SP spawn-select UI.
    pub fn singleplayer(name: impl Into<String>, lifestyle: crate::simulation::faction::Lifestyle) -> Self {
        Self {
            primary_start: None,
            slots: vec![PlayerStartSlot::singleplayer(name, lifestyle)],
        }
    }

    /// First slot whose `client_id` matches the supplied id (i.e. "find my
    /// own slot"). Used by `spawn_population` to set `PlayerFaction`.
    pub fn slot_for_client(&self, client_id: u64) -> Option<&PlayerStartSlot> {
        self.slots.iter().find(|s| s.client_id == client_id)
    }
}

/// User-facing seed driving globe + per-tile terrain generation. The
/// spawn-select UI exposes this as an editable field with Apply / Reroll
/// buttons; firing `RegenerateWorldRequest` rebuilds the `Globe` and
/// `WorldGen` resources from this seed.
#[derive(Resource, Clone, Copy, Debug)]
pub struct WorldSeed(pub u64);

impl Default for WorldSeed {
    fn default() -> Self {
        Self(42)
    }
}

/// Fired by the spawn-select UI to rebuild the world from `WorldSeed`.
#[derive(Event)]
pub struct RegenerateWorldRequest;

/// Civic seeding density at game start. Controls how aggressively
/// `seed_starting_buildings_system` bypasses the runtime `(Era, peak_pop)`
/// civic milestone gates.
///
/// - `Founder` — gates stay live; Neolithic-20 starts skip Market/Barracks/
///   Monument until they hit the population thresholds.
/// - `Established` (default) — current behaviour: every era-appropriate
///   civic (Granary/Shrine/Market/Barracks/Monument) seeds regardless of
///   pop, mirroring "society in progress".
/// - `Developed` — `Established` + explicit override that always emits
///   Monument/Barracks/Market once `era >= Chalcolithic`, even with a
///   smaller pop than the Bronze milestones would normally require.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSettlementMaturity {
    Founder,
    Established,
    Developed,
}

/// Economy preset selected at game start; applied to every faction's
/// `economic_policy` map by `spawn_population`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EconomyPreset {
    /// Default — empty `economic_policy` map (chief allocates everything).
    Subsistence,
    /// Chief still allocates food/wood/stone; tools/cloth/luxury resources
    /// flip `private_actors_allowed = true` so households can craft them.
    Mixed,
    /// Full `ResourceControlPolicy::capitalist()` on every catalog resource.
    Market,
}

/// Player-configurable game-start options written by the spawn-select UI
/// and read once by `spawn_population` (and `seed_starting_buildings`).
#[derive(Resource, Clone, Copy, Debug)]
pub struct GameStartOptions {
    pub era: crate::simulation::technology::Era,
    /// Number of starting members in the player faction (`group_idx == 0`).
    /// Other factions stay at the hardcoded `GROUP_SIZE`.
    pub player_population: u32,
    pub economy: EconomyPreset,
    /// Whether to pre-build era-appropriate structures around each faction's
    /// home tile. Set to false in sandbox mode.
    pub seed_buildings: bool,
    /// Faction archetype for the player faction. Nomadic skips Settlement
    /// founding, plot carving, and FactionStorageTile spawn; structures use
    /// the pack/deploy cycle and the band migrates seasonally.
    pub lifestyle: crate::simulation::faction::Lifestyle,
    /// Civic seeding density. See `StartSettlementMaturity`.
    pub maturity: StartSettlementMaturity,
}

impl Default for GameStartOptions {
    fn default() -> Self {
        Self {
            era: crate::simulation::technology::Era::Paleolithic,
            player_population: 20,
            economy: EconomyPreset::Subsistence,
            seed_buildings: true,
            lifestyle: crate::simulation::faction::Lifestyle::Settled,
            maturity: StartSettlementMaturity::Established,
        }
    }
}

/// Plugin owning the lifecycle resources (states + PendingStarts/Spawn
/// + GameStartOptions + WorldSeed) and the PendingSpawn compat sync.
///
/// `legacy_pending_spawn_compat_system` runs in `PreUpdate` and mirrors
/// `PendingStarts.primary_start` (falling back to `slots[0].megachunk`)
/// onto `PendingSpawn.0`, so the legacy single-slot readers
/// (`spawn_world_system`, `position_camera_for_spawn`, abstract-faction
/// seeders, the test fixture) keep working unchanged while multi-start
/// migration proceeds incrementally.
pub struct GameStatePlugin;

impl Plugin for GameStatePlugin {
    fn build(&self, app: &mut App) {
        // Note: WorldSeed + RegenerateWorldRequest are owned by WorldPlugin
        // (WorldSeed must be inserted *before* the globe load that reads it
        // in WorldPlugin::build, so we don't duplicate the insert here).
        app.init_state::<GameState>()
            .add_sub_state::<SimulationState>()
            .insert_resource(PendingSpawn::default())
            .insert_resource(PendingStarts::default())
            .insert_resource(GameStartOptions::default())
            .add_systems(PreUpdate, legacy_pending_spawn_compat_system);
    }
}

/// Mirror `PendingStarts.primary_start` → `PendingSpawn.0` every PreUpdate
/// so legacy single-slot readers keep working.
pub fn legacy_pending_spawn_compat_system(
    starts: Res<PendingStarts>,
    mut pending: ResMut<PendingSpawn>,
) {
    let resolved = starts
        .primary_start
        .or_else(|| starts.slots.first().and_then(|s| s.megachunk));
    if pending.0 != resolved {
        pending.0 = resolved;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::faction::Lifestyle;
    use bevy::state::app::StatesPlugin;

    #[test]
    fn pending_starts_compat_legacy_single_slot() {
        let mut app = App::new();
        app.add_plugins(StatesPlugin);
        app.add_plugins(GameStatePlugin);
        // SP: one slot with a megachunk set.
        let mut starts = PendingStarts::singleplayer("p1", Lifestyle::Settled);
        starts.slots[0].megachunk = Some((4, 7));
        app.insert_resource(starts);
        // Run PreUpdate once.
        app.update();
        let ps = app.world().resource::<PendingSpawn>();
        assert_eq!(ps.0, Some((4, 7)));
    }

    #[test]
    fn pending_starts_compat_primary_start_wins() {
        let mut app = App::new();
        app.add_plugins(StatesPlugin);
        app.add_plugins(GameStatePlugin);
        let mut starts = PendingStarts::singleplayer("p1", Lifestyle::Settled);
        starts.primary_start = Some((1, 2));
        starts.slots[0].megachunk = Some((9, 9));
        app.insert_resource(starts);
        app.update();
        let ps = app.world().resource::<PendingSpawn>();
        // primary_start takes precedence (it's the camera anchor for the
        // local client — clients land here regardless of which slot is 0).
        assert_eq!(ps.0, Some((1, 2)));
    }
}
