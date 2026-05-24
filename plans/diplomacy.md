# Diplomacy And Territory

## Summary

Add a derived tile-level `TerritoryMap`, a full diplomacy/reputation ledger, territory trespass handling, defensive responses, and a player-facing diplomacy screen. Territory is precise in active simulation areas and projected coarsely on the world map for abstract/offscreen factions.

## Key Changes

- Add `src/simulation/territory.rs` with:
  - `TerritoryMap { cells, by_faction, version }`
  - `TerritoryCell { owner, state: Claimed | Contested | Unclaimed, score, runner_up }`
  - `TerritoryStats { claimed_tiles, contested_tiles }`
  - Influence anchors from live `Settlement`s and pitched `Camp`s.
- Compute influence on an Economy cadence around each anchor only:
  - Settlement radius = era base plus `sqrt(peak_population)` bonus, capped by era.
  - Camp radius = smaller temporary radius while pitched.
  - Tile owner = highest score when above threshold and ahead by contest margin.
  - Overlaps become `Contested` when scores are close.
- Add `src/simulation/diplomacy.rs` with:
  - `DiplomacyLedger` resource keyed by sorted `FactionPair`.
  - `DiplomaticRelation { reputation, treaties, last_contact_tick }`.
  - `Reputation { trust, fear, grievance, familiarity }`.
  - Treaties: `TradePact`, `Alliance`, `War`; war is exclusive and cancels trade/alliance.
  - Incidents: `Trespass`, `IgnoredWarning`, `Attack`, `Raid`, `TradeCompleted`, `Aid`, `SharedEnemy`.
- Add player commands:
  - `SendDiplomacyProposal { faction_id, target, proposal }`
  - `RespondDiplomacyProposal { faction_id, proposal_id, response }`
  - Validate through `ControlledFactions` like other faction-level commands.
- Trespass behavior:
  - Same faction/root, alliance, and approved trade access are legal.
  - Neutral strangers receive one warning, then repeated incidents add grievance.
  - War enemies in owned territory are hostile immediately.
- Protection behavior:
  - Add `TerritoryDefenseTarget { tile, intruder, expires_tick }`.
  - Extend `AgentGoal::Defend` dispatch to prefer this target over home.
  - Owner factions assign nearby Hunters first, then capable adults, with party size scaling by era.
  - Defenders warn/chase neutral trespassers; they attack war enemies or repeated violators.
- Integrate diplomacy with raids:
  - Raid target selection excludes allies and trade partners unless already at war.
  - Raids create grievance, break trade/alliance, and can set `War`.
  - AI diplomacy may propose trade/alliance/peace or declare war from reputation scores.

## UI

- Add `src/ui/diplomacy_panel.rs` and register it in `UiPlugin`.
- Add a HUD “Diplomacy” button.
- Screen layout:
  - Left: known factions, current treaty state, attitude summary.
  - Center: reputation tracks, recent incidents, territory/trespass status.
  - Right: proposals and actions: propose trade, propose alliance, propose peace, declare war, respond to incoming messages.
- Add diplomacy message entries to `ActivityLogEvent`, including trespass warnings and treaty changes.
- Add a world-map territory overlay toggle that tints claimed/contested regions by faction color.

## Tests

- Territory unit tests:
  - Radius grows by era and population.
  - Stronger overlapping settlement wins border tiles.
  - Close overlap produces contested tiles.
  - Pitched nomad camp claims territory; packed camp does not.
- Diplomacy unit tests:
  - War cancels trade/alliance.
  - Trespass without access creates warning then grievance.
  - Alliance permits territory entry.
  - Reputation incidents decay deterministically.
- Integration tests with `test_fixture`:
  - AI defender receives `TerritoryDefenseTarget` after trespass.
  - Repeated trespass escalates to hostile response.
  - Raid creates war/grievance and blocks future trade until peace.
  - Player proposal commands are rejected for uncontrolled factions.
- Run `cargo test --bin civgame`.

## Assumptions

- No new crates.
- Territory is derived data, not a terrain mutation.
- Tile-level claims are only maintained for active/local simulation; abstract factions keep coarse globe-cell projection until materialized.
- Trade pact grants legal market/road access, alliance grants full territorial access, war grants no warning.
- Update root `AGENTS.md` plus `src/simulation/CLAUDE.md` and `src/ui/CLAUDE.md` with the new diplomacy/territory behavior.
