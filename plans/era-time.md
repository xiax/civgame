# Civic Time Use, Farm Rushes, And Festivals

## Summary
Add a civic-calendar layer that changes how people spend ordinary time by era, season, and institution. It should **reuse the existing mass planting/harvest pipeline** instead of duplicating it: Spring/Autumn farm days raise pressure on the existing `JobKind::Farm` / `JobProgress::FieldWork` postings. Festivals get a new assembly/routing layer so people actually gather at a shrine, monument, hearth, or home before receiving ritual/social benefits.

## Key Changes
- Add `src/simulation/time_use.rs` with:
  - `CivicDayKind`: `Normal`, `PlantingRush`, `HarvestRush`, `MarketDay`, `CorveeDay`, `RitualFestival`, `LectureDay`, `DrillDay`, `EmergencySubsistence`.
  - `FactionCivicCalendar`: per-faction current day kind, anchor tile, start/end ticks, and deterministic seed.
  - `FestivalAssembly` resource plus `FestivalDuty` component for invited participants.
  - Helpers to choose civic anchors: Monument > Shrine > Campfire/Hearth > `home_tile`.

- Farm integration:
  - Do **not** add new planting/harvest tasks.
  - Keep existing seasonal postings from `chief_job_posting_system`: Spring `Prepare`/`Plant`, Autumn `Harvest`, Winter none.
  - On `PlantingRush` / `HarvestRush`, boost `WorkforceBudget.farm`, Farm posting priority, and `FarmWorkScorer` for private plots.
  - Keep existing claim scaling from `posting_target_workers(FieldWork)` so larger plot work naturally admits more workers.
  - Treat `EmergencySubsistence` as higher priority than civic farm days.

- Festival integration:
  - Replace the season-edge “benefit whoever is already nearby” behavior with a two-phase event:
    1. **Muster window:** select eligible participants and route them to open ring slots around the civic anchor.
    2. **Festival window:** apply social/willpower/esteem relief only to attendees within radius.
  - Skip people who are starving, severely thirsty, injured, drafted, commanded, migrating, packing/pitching camp, or holding critical farm/haul/build claims.
  - Add a job-claim skip for `FestivalDuty` so invited participants are not immediately pulled back into work.
  - Record `invited`, `arrived`, and `benefited` counts in the ritual/debug state.

- Institutional cadence:
  - `EmergencySubsistence` wins during severe food shortage, raids, or migration crisis.
  - `PlantingRush`: Spring, when state-owned or private agricultural plots have prepare/plant work.
  - `HarvestRush`: Autumn, when plot-scoped mature grain exists.
  - `RitualFestival`: season transition or ceremonial culture day, gated by surplus food for feast-strength benefits.
  - `MarketDay`: Chalcolithic+ with Market or long-distance trade.
  - `CorveeDay`: Neolithic+ with public projects waiting.
  - `LectureDay`: tech gaps plus teacher/tablet/book availability.
  - `DrillDay`: Bronze+ with Barracks or professional army tech.

## Implementation Notes
- Schedule civic-calendar selection in `SimulationSet::Economy` before workforce budgeting and chief posting.
- Thread `FactionCivicCalendar` into `workforce_budget_system`, `chief_job_posting_system`, and goal scoring context.
- Add a small festival dispatcher in `ParallelB` that routes `FestivalDuty` agents to reserved assembly slots.
- Keep survival, care, combat, player commands, migration, and existing durable maintenance behavior preemptive.

## Test Plan
- Unit-test civic-day selection precedence and deterministic outputs.
- Farm tests:
  - Spring rush increases farm workforce share and Farm claim count using existing `FieldWork`.
  - Autumn rush prefers Harvest postings without creating duplicate jobs.
  - Winter produces no farm rush.
- Festival tests:
  - Participants receive no benefit until they arrive near the anchor.
  - `FestivalDuty` agents do not claim jobs mid-muster.
  - Starving/injured/commanded/migrating agents are excluded.
  - Existing ritual pulse still records debug events, now with invited/arrived/benefited counts.

## Assumptions
- No new crates.
- The first pass applies to all factions.
- Festival benefits are abstracted as need/esteem relief; no food consumption unless the faction has safe surplus.
- Mass farm labor remains owned by the existing `FarmSeasonPhase` / `FieldWork` systems.
