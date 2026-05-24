# Neolithic Hearth Seeding Fix

## Summary
Fix settled Neolithic game starts so they seed **at most one public/civic hearth** near the faction center, while keeping **domestic hearths only inside Longhouses**. The old population-scaled hearth coverage (`ceil(pop/8)`) should stop applying to settled Neolithic+ factions. Paleo/Meso band camps and nomadic camps keep their multi-hearth camp logic.

The Neolithic seed-time hearth tier should use the era profile (`Ringed`) instead of the tech ladder that currently upgrades to `Lined` because `FIRED_POTTERY` is a Neolithic tech.

## Interface And Type Changes
- In `src/simulation/construction.rs`, add:
  - `HearthRole::{Camp, Civic, Domestic}` with default `Camp`.
  - `Campfire { tier: HearthTier, role: HearthRole }`.
  - `CampfireEntry { entity: Entity, role: HearthRole }`.
  - Change `CampfireMap` from `AHashMap<(i32, i32), Entity>` to `AHashMap<(i32, i32), CampfireEntry>`.
- Add explicit constructors/helpers for spawning and map insertion, so every campfire spawn site sets both `tier` and `role`.
- Add `Blueprint.hearth_role: Option<HearthRole>` plus `with_hearth_role(...)`; construction completion defaults unmarked manual `BuildSiteKind::Campfire` blueprints to `HearthRole::Civic`.
- Replace the walled-house plan tuple with a small plan-entry struct carrying `kind`, `tile`, `door_edge`, and optional `hearth_role`, so Longhouse interior hearths propagate as `Domestic` through immediate and deferred terraform paths.

## Implementation Changes
- In `organic_settlement::append_pressures_for_faction`, replace Neolithic population-scaled hearth coverage with role-aware public hearth targeting:
  - Paleo/Meso: keep existing camp target `ceil(members / 6)` and count `HearthRole::Camp`.
  - Neolithic/Chalcolithic/Bronze settled factions: target `1` and count only `HearthRole::Civic`.
  - Rename the pressure reason from `"hearth coverage"` to `"public hearth"`.
- In `pressure_to_intent`, mark `SettlementPressureKind::Hearth` intents as:
  - `Camp` for pre-Neolithic camp-style pressure.
  - `Civic` for settled Neolithic+ pressure.
- In seed-time direct stamping:
  - Pass the full `SeedConstructionProfile` into `seed_apply_intent` / `spawn_seeded_structure_at_tile`.
  - For seed `BuildSiteKind::Campfire`, use `profile.hearth_tier`, not `best_hearth_for(seed_techs)`.
  - Keep Longhouse interior hearths using the same seed profile, so Neolithic Longhouses stamp `Ringed` domestic hearths and Bronze stamps `Lined`.
- Mark spawn sources explicitly:
  - Paleo/Meso settled band-camp seeder: `HearthRole::Camp`.
  - `seed_nomadic_camp` and nomad re-pitch/unpack paths: `HearthRole::Camp`.
  - Organic standalone Neolithic+ hearth pressure: `HearthRole::Civic`.
  - Longhouse interior hearths: `HearthRole::Domestic`.
- Update all `CampfireMap` call sites to use `entry.entity` when despawning and `entry.role` when counting/filtering. Existing nearest-hearth behavior for warmth, hunting muster, butchering, UI occupancy, and nomad packing should continue to iterate keys exactly as before unless role-specific logic is needed.
- Update docs:
  - Root `AGENTS.md` game-start seeding note: Neolithic+ gets at most one civic hearth; extra fire comes from Longhouse interiors.
  - `src/simulation/CLAUDE.md` hearth pacing and tier notes: no population-scaled Neolithic public hearths; seed-time Neo hearth tier is Ringed, Chalco/Bronze Lined.

## Test Plan
- Add unit coverage for desired hearth targets:
  - Paleo/Meso 20-pop still targets `ceil(20/6)`.
  - Settled Neolithic 20-pop targets `1` civic hearth.
  - Chalcolithic/Bronze settled starts also target `1` civic hearth.
- Add/adjust OnEnter seed tests in `src/simulation/test_fixture.rs`:
  - Neolithic 20-pop start has exactly one `HearthRole::Civic` campfire near the player home/commons.
  - That civic Neolithic hearth is `HearthTier::Ringed`, not `Lined`.
  - Longhouse interior campfires, when present, are `HearthRole::Domestic` and do not count toward civic hearth pressure.
  - Paleo/Meso starts still produce camp-role multi-hearth band camps.
  - Bronze starts still produce at least one `Lined` hearth where appropriate.
- Update the runtime regression test currently documenting the old “hearth cap removed” behavior so it asserts:
  - no standalone outdoor Neolithic bed blueprints,
  - no more than one civic hearth around the settled Neolithic center,
  - domestic Longhouse hearths may exist separately.
- Verification commands:
  - `cargo test --bin civgame seed_profile_reproduces_era_table`
  - `cargo test --bin civgame onenter_era_seeding`
  - `cargo test --bin civgame neolithic_runtime_no_paleo_beds_or_excess_hearths`
  - `cargo test --bin civgame`

## Assumptions
- Settled Neolithic should target **one public/civic hearth maximum**, as selected.
- Huts remain bed-only; no hut resizing or outdoor household oven feature in this fix.
- This plan fixes game-start realism and pressure counting without changing the broader `FIRED_POTTERY` tech unlock semantics for unrelated runtime/manual construction.
- No new crates are needed.
