# Knowledge-Based Technology Adoption — Comprehensive Plan

## Context

The current tech system has the right components but the wrong glue:

- `PersonKnowledge` tracks `aware` + `learned` + `study_progress` per person.
- `FactionTechs` is **already** a derived cache — `sync_faction_techs_from_chief_system` mirrors the chief's `aware` bitset each Economy tick.
- Because that cache is one person's awareness, the faction "has a tech" the moment the chief hears of it. Civic gates, building tiers, settlement style, and the era ladder all flip on chief‑rumor.
- `discovery_system` lands directly in `Learned` (not `Aware`), so a single trigger event grants mastery, skipping study.
- `seeded_through_era` flips every era‑prior tech to Aware+Learned for *every* founder — no specialists, scribes, or institutional preservation.

Result: a chief rumor unlocks bronze beds for everyone; chief death can roll back the faction even if dozens of practitioners remain. This refactor relocates "what tech the faction has" from chief-awareness to *community adoption derived from real practice and infrastructure*.

## Design

### Concepts

```
PersonKnowledge (per-person aware / learned / study_progress)
        │ low-cadence derivation
        ▼
FactionData.tech_adoption: [AdoptionStage; TECH_COUNT]
        │ helpers
        ├── can_direct_tech(faction, tech)    → chief is Aware
        ├── community_has_adopted(faction, tech) → stage ≥ Adopted
        └── worker_can_perform(person, tech)  → has_learned
```

`FactionTechs(u64)` stays as the chief‑aware bitset for tight loops; renamed `ChiefAwareness` for clarity.

### `AdoptionScale` (in `technology.rs`)

- `Personal`: HUNTING_SPEAR, HORSEBACK_RIDING, BOW_AND_ARROW
- `Household`: FOOD_SMOKING, FIRED_POTTERY, LOOM_WEAVING
- `Subsistence`: CROP_CULTIVATION, ANIMAL_HUSBANDRY, FIRE_MAKING
- `Specialist`: FLINT_KNAPPING, COPPER_WORKING, BRONZE_CASTING, COPPER_TOOLS, BRONZE_TOOLS, BRONZE_WEAPONS
- `MilitaryTransport`: HORSE_TAMING
- `Institutional`: PERM_SETTLEMENT, SACRED_RITUAL, LONG_DIST_TRADE, GRANARY, CUNEIFORM_WRITING, BOOK, CITY_STATE_ORG, MONUMENTAL_BUILDING, PORTABLE_DWELLINGS

Unclassified default: `Household`. Mapping table sits beside `TECH_TREE`.

### `AdoptionStage`

`Unknown(0) Rumored(1) Demonstrated(2) Practiced(3) Adopted(4) Institutionalized(5)` as `#[repr(u8)]`. Stored as `[AdoptionStage; TECH_COUNT]` on `FactionData`.

### Thresholds

| Scale | Practiced | Adopted | Institutionalized |
|---|---|---|---|
| Personal | ≥1 learned | ≥1 learned | Adopted ≥ 1 game-year |
| Household | ≥1 learned | `max(2, 20% adults)` learned OR ≥50% households with practitioner/artifact | Adopted ≥ 1 game-year |
| Subsistence | ≥1 learned + recent use | ≥30% households used this season | Adopted ≥ 1 game-year + artifact present |
| Specialist | ≥1 learned + station/recent use | ≥1 learned + station present + ≥3 successful crafts in 30 days | Adopted ≥ 1 game-year + ≥2 living practitioners |
| MilitaryTransport | ≥1 learned + equipment present | ≥2 learned OR 1 trainer + ≥3 deployments in 60 days | Adopted ≥ 1 game-year |
| Institutional | chief/scribe learned + prereqs adopted | chief/scribe learned + prereqs adopted + civic building present + population ≥ scale | Adopted ≥ 1 game-year + ≥1 tablet/book OR ≥2 scribes |

`RecentTechUse`: per-(faction, tech) ring buffer of last 8 timestamps; entries older than 60 days dropped on read.

### Era is a label, gating is per-tech

`current_era` becomes a UI label only ("highest era E where ≥50% of E's techs are `community_has_adopted`"). Every tier/civic decision queries the specific `TechId` via `community_has_adopted`. `civic_milestones.rs` keys flip from `(Era, peak_pop)` to `(&[TechId], peak_pop)`.

## Implementation phases

### Phase 0 — Fix discovery (knowledge.rs + activity_log.rs)

`discovery_system` on success: set `Aware`, add `complexity × 1200` to `study_progress`. Emit `ActivityEntryKind::TechInsight`. `study_system` + `teaching.rs` remain the only paths flipping `Learned` (besides seeding).

### Phase 1 — Adoption metadata & cache

New `technology_adoption.rs`: enums, `tech_scale`, `AdoptionThresholds`, `RecentTechUse`, `derive_tech_adoption_system` (Economy schedule, every 900 ticks). Extend `FactionData` with `tech_adoption: [AdoptionStage; TECH_COUNT]` + `recent_tech_use: AHashMap<TechId, RecentTechUse>`. No behavior change yet.

### Phase 2 — Helpers + migrate call sites

Add `can_direct_tech` / `community_has_adopted` / `worker_can_perform`. Migrate:
- `construction.rs` (~15): material/tier gates → per-tech `community_has_adopted`. Rewrite `best_X_for` internals.
- `settlement.rs` (~5) + `organic_settlement.rs` (~2): civic placement → `community_has_adopted`. StreetSpine selection → per-tech.
- `civic_milestones.rs`: key `(Era, peak_pop)` → `(&[TechId], peak_pop)`.
- `technology.rs::current_era`: UI label only.
- `htn.rs` / `crafting.rs`: unchanged (already per-person Learned).
- `jobs.rs` / `faction.rs`: chief decisions → `can_direct_tech`.

### Phase 3 — Scoped seeding

Replace `seeded_through_era` callers with three seeders:
- `seed_common_through_era(era)`: Personal+Household+Subsistence → all adults Aware+Learned.
- `seed_specialist(era)`: Specialist → `max(1, members/8)` random adults Aware+Learned, others Aware only.
- `seed_institutional(era, chief, scribe)`: Institutional → chief/scribe Aware+Learned, others Aware only.

Adoption stages derive from this on the next tick.

### Phase 4 — Decay

In `derive_tech_adoption_system`: if conditions for current stage lapse, downgrade one stage per game-day max (cooldown via `stage_changed_at_tick`). Institutional→Adopted (no scribe+artifact); Adopted→Practiced (no learned practitioner); Practiced→Demonstrated (no use in 1 year).

### Phase 5 — UI + activity log

- Tech panel: `AdoptionStage` chip per tech + missing-threshold tooltip.
- Activity log: `TechInsight` (P0), `TechAdopted`, `TechInstitutionalized` (emitted on stage transitions).

### Phase 6 — Tests + docs

Unit: stage derivation per scale; discovery=Aware regression; current_era from adoption.
Integration: Neolithic faction + chief Aware(BRONZE) → no bronze beds. Bronze faction + workbench + specialist → bronze beds. Specialists die → adoption decays. Newborn inherits Aware only.
Docs: update `CLAUDE.md` + `src/simulation/CLAUDE.md`.

## Critical files

| File | Change |
|---|---|
| `src/simulation/technology.rs` | `AdoptionScale`, scale table; `current_era` → UI label |
| `src/simulation/technology_adoption.rs` | **New.** Stage, thresholds, derive system, helpers |
| `src/simulation/civic_milestones.rs` | Key by `&[TechId]` instead of `Era` |
| `src/simulation/knowledge.rs` | P0 discovery fix |
| `src/simulation/faction.rs` | `tech_adoption`, `recent_tech_use` fields; rename `FactionTechs` → `ChiefAwareness` |
| `src/simulation/person.rs` | Scoped seeders replace `seeded_through_era` |
| `src/simulation/construction.rs` | ~15 sites + `best_X_for` rewrites |
| `src/simulation/settlement.rs` | ~5 sites |
| `src/simulation/organic_settlement.rs` | ~2 sites |
| `src/simulation/jobs.rs` | Chief posting via `can_direct_tech` |
| `src/ui/tech_panel.rs` | Adoption stage chip + tooltip |
| `src/ui/activity_log.rs` | `TechInsight`, `TechAdopted`, `TechInstitutionalized` |
| `CLAUDE.md`, `src/simulation/CLAUDE.md` | Doc updates |

## Reuse

`PersonKnowledge::{is_aware, has_learned, complexity_used, add_study_progress, try_learn}`; `learning_slowdown`; `tech_def`; `recipe_for`, `faction_can_build`; `ActivityLogEvent`; `SimulationSet::Economy`.

## Verification

1. `cargo check` green per phase.
2. `cargo test --bin civgame` passes new tests.
3. `cargo run` (no sandbox): tech panel chips populate; insight log entry appears on discovery; Neolithic stays Neolithic despite chief rumor of bronze; workbench+specialist flips to bronze beds.
4. Long run with mortality → no era flicker; decay observable.

## Performance

`derive_tech_adoption_system` every 900 ticks: O(members) per faction + 44 stage evals = negligible (≤1k entity reads, ≤880 evals per pass at 20×50 scale).

## Out of scope

Artifact-economy details; new techs; per-settlement granularity; inter-faction knowledge diplomacy.
