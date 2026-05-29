**Realistic Poor Shelter And Camp Layout**

**STATUS: shipped 2026-05-28.** Emergent single-tile clusters (not a composite kind); sleeping mats give graduated 1.25× recovery. Key sites: `construction::{BuildSiteKind::SleepingMat/LightShelter, SleepingMatMaterial, LightShelterMaterial, BedTier::SleepingMat, ShelterTier::LeanTo, ShelterMap, select_poor_shelter_material, TentShelter on_add/on_remove hooks}`, `organic_settlement::{poor_shelter_intent, find_poor_shelter_site}` (replaces `find_emergency_bed_tile`), `sleep::sleep_task_system` (tier-aware), `needs::tick_needs_system` (ShelterMap relief), sprites `building_sleeping_mat`/`building_lean_to`, wire `structure_kind_wire_from_label` + `PROTOCOL_VERSION=9`. 6 unit tests in `construction::tests`. Rendering is complete: dedicated procedural sprites for sleeping mat (`building_sleeping_mat`), lean-to (`building_lean_to`), tent (`building_tent`), and a new yurt (`building_yurt`); finished structures render via `spawn_bed_sprites` (mat, tier-aware) + new `spawn_tent_shelter_sprites` (lean-to/tent/yurt, per-`ShelterTier`); blueprint ghosts use the same sprites. Deviations: `MaterialPolicyDef::LightShelter`/`CandidateReason::PoorShelter` left as-is (pre-existing unused scaffolding, not wired); broader integration/regression coverage beyond the one end-to-end test + 6 unit tests deferred to manual verification (`cargo run`).

**Summary**
Replace the settled-age emergency “bare Bed in a corner” fallback with poor-housing primitives: sleeping mats plus lightweight shelters placed as small, spaced household clusters. Keep Paleolithic/Mesolithic open camps as a distinct hearth-centered lifestyle, but make the code paths explicit so open-camp logic does not leak into Neolithic+ settled villages.

**Current Model**
- `Paleolithic` / `Mesolithic` seed paths place `Camp` hearths, then simple beds/bedrolls around each hearth. This is acceptable for mobile or open-air forager camps, but it is a stylized ring.
- `Neolithic+` normal shelter pressure becomes `Hut` or `Longhouse`.
- `Neolithic+` emergency shelter currently becomes `BuildSiteKind::Bed` when wall materials are unavailable. This is the unrealistic path causing exposed bed blueprints to bunch in one area.
- Shelter need is currently relieved by enclosure and nearby campfires, not by `TentShelter`, so lightweight shelters are mostly visual/pack-deploy markers today.

**Public Types And Interfaces**
- Add `BuildSiteKind::SleepingMat(SleepingMatMaterial)` and `BuildSiteKind::LightShelter(LightShelterMaterial)` in `/Users/xiao1/civgame/src/simulation/construction.rs`.
- Add:
  - `SleepingMatMaterial::{BareGround, Reed, Thatch, Hide}`
  - `LightShelterMaterial::{ReedScreen, ThatchLeanTo, BrushLeanTo}`
  - `ShelterTier::LeanTo` alongside existing `Tent` / `Yurt`
- Extend `BedTier` with `SleepingMat` and update sleep recovery:
  - no bed: `1.0x`
  - sleeping mat: `1.25x`
  - crude/framed/carved beds: keep existing practical behavior at `2.0x` unless a later balance pass changes tier bonuses
- Extend `TentShelter` into a real shelter component:
  - `tier`
  - `owning_faction: Option<u32>`
  - `relief_radius: u8`
  - `relief_per_day: f32`
  - `capacity: u8`
- Add a `ShelterMap` resource keyed by tile, populated for `LightShelter`, `Tent`, and `Yurt`, so `needs.rs` can cheaply apply shelter relief.

**Implementation Changes**
- Replace emergency settled shelter intent:
  - In `organic_settlement::pressure_to_intent`, do not map `WallSelection::EmergencyShelter` to `OrganicBuildKind::Single(BuildSiteKind::Bed)`.
  - Add `OrganicBuildKind::PoorShelterCluster { shelter: Option<LightShelterMaterial>, mat: SleepingMatMaterial, beds: u8 }`.
  - Use this only for `PERM_SETTLEMENT` factions when normal Hut/Longhouse material selection returns `EmergencyShelter`.
- Add poor-shelter material selection:
  - Prefer `ReedScreen` + `Reed` mat if reeds are available/tight/procurable.
  - Else prefer `ThatchLeanTo` + `Thatch` mat if thatch is available/tight/procurable.
  - Else prefer `BrushLeanTo` if wood is available/tight/procurable.
  - Else use `BareGround` sleeping mats with no shelter entity as the absolute last resort.
  - Include pending blueprints when deciding “available enough” so the chief does not over-queue the same scarce input.
- Add poor-shelter cluster emission:
  - Cluster shape: one optional `LightShelter` tile plus 1-2 adjacent `SleepingMat` tiles.
  - If there is no `Campfire` within 5 tiles and wood is not unavailable, allow one small `Campfire` nearby; otherwise do not force a fire.
  - Finalized `SleepingMat` inserts a `Bed` with `BedTier::SleepingMat`, so `HomeBed` assignment still works.
  - Finalized `LightShelter` inserts `TentShelter { tier: LeanTo, relief_radius: 1, capacity: 2 }` and `StructureLabel`.
- Replace `find_emergency_bed_tile` with a new poor-housing site picker:
  - Inputs include built beds, pending sleeping mats/beds, built shelters, pending shelters, roads, doormats, `StructureIndex`, and settlement brain parcels.
  - Prefer Residential parcels or frontage-adjacent edge lots.
  - Fall back to an era-specific poor-housing annulus only when parcel placement fails.
  - Score by reachability, distance to road/home, avoiding commons/fields/roads, and distance from existing poor shelters/beds.
  - Include pending blueprints in spacing, so multiple queued shelters spread before anything is completed.
- Use era-specific spacing:
  - Neolithic: prefer at least `5` Chebyshev tiles from existing/pending poor-shelter sleep spots.
  - Chalcolithic: prefer `4`.
  - Bronze Age: prefer `3`, allowing denser overflow housing.
  - Relax one step at a time down to `1` only if no valid site exists.
- Keep open camps separate:
  - `Paleolithic` / `Mesolithic` non-permanent shelter remains hearth-centered.
  - Clean up comments and function names so this path is described as open camp bedding, not generic emergency housing.
  - Optional small realism improvement: make hearth-bed placement choose clustered arcs biased away from water/impassable terrain instead of a perfect ring, but do not mix it with settled poor housing.
- Make lightweight shelters mechanically useful:
  - In `/Users/xiao1/civgame/src/simulation/needs.rs`, apply shelter relief from `ShelterMap` before or alongside campfire warmth.
  - Use the strongest shelter covering the agent’s current tile; do not stack multiple lean-tos.
  - Keep enclosure relief unchanged, so proper houses remain better than lean-tos.
- Rendering/UI:
  - Add simple procedural pixel sprites for sleeping mats and lean-tos in the existing pixel-art style.
  - Blueprint ghost for `SleepingMat` should look like a mat, not a bed.
  - Blueprint ghost for `LightShelter` should look like a lean-to/windbreak, not a wall.
  - Add labels for hover/activity log/orders if these are player-buildable; otherwise labels still need to display for chief-generated blueprints.
- Docs:
  - Update `/Users/xiao1/civgame/AGENTS.md` settlement construction notes.
  - Update `/Users/xiao1/civgame/src/simulation/CLAUDE.md` shelter, needs, and organic settlement sections.

**Test Plan**
- Unit tests:
  - Poor-shelter selector chooses reed, thatch, brush, then bare-ground fallback in order.
  - `SleepingMat` finalizes into `BedTier::SleepingMat` and enters `BedMap`.
  - `LightShelter` finalizes into `ShelterMap` and `StructureIndex`.
  - `sleep_task_system` gives `1.25x` recovery on sleeping mats and preserves existing `2.0x` behavior for normal beds.
  - `needs::tick_needs_system` reduces shelter need when an agent stands under a lean-to/tent/yurt and does nothing outside radius.
- Regression tests:
  - Neolithic runtime bed deficit with unavailable wall materials emits `SleepingMat`/`LightShelter`, not standalone `BuildSiteKind::Bed`.
  - Multiple pending emergency poor shelters spread out using pending blueprint positions, not just finished structures.
  - Chalcolithic and Bronze emergency shelter also spread, with denser allowed spacing than Neolithic.
  - Paleolithic/Mesolithic starts still produce camp hearths plus open camp bedding and do not emit settled poor shelters.
  - Normal Neolithic Hut/Longhouse placement is unchanged when wall materials are available.
- Run:
  - `cargo test --bin civgame poor_shelter`
  - `cargo test --bin civgame neolithic_emergency`
  - `cargo test --bin civgame`
  - `cargo check`

**Assumptions**
- Settled poor housing should be visibly poor but still plausibly sheltered: mats, lean-tos, reed screens, and edge-of-village clusters.
- Bare exposed sleeping spots are allowed only as an absolute last resort and should render as ground mats/spots, not wooden beds.
- Open camps remain historically appropriate for mobile forager bands; the main realism bug is settled Neolithic+ villages reverting to bare bed blueprints.
