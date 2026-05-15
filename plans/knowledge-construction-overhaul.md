# Knowledge-Posted Construction Overhaul

## Summary
- Move construction authorization from community `AdoptionStage::Adopted` to the **Learned knowledge of the blueprint/order poster**.
- Add `Profession::Architect` as a real labor role. Chiefs remain valid posters when they personally know the build tech; otherwise they can rely on hired architects.
- Manual player build orders use the player faction’s chief/architect poster pool, not the selected worker’s knowledge.
- Settlement seeding stops impersonating technology adoption. It uses `GameStartOptions.era` to choose era-appropriate starting structures and never validates blueprint/posting gates.

## Key Interfaces
- Add construction poster metadata to `Blueprint`:
  - `posted_by: Option<Entity>`
  - `design_techs: FactionTechs`, a snapshot of the poster’s `PersonKnowledge.learned` at posting time.
- Add helper APIs in the construction layer:
  - `build_kind_required_tech(BuildSiteKind) -> Option<TechId>`
  - `poster_can_post_build_kind(kind, knowledge) -> bool`
  - `poster_can_post_intent(intent, knowledge) -> bool`
  - `intent_required_kinds(intent) -> small vec/array of BuildSiteKind`
  - `select_poster_for_intent(faction_id, intent, poster_pool) -> Option<PosterCapability>`
- Add `ConstructionPosterPool` resource refreshed before construction planning:
  - includes each faction’s chief plus all `Profession::Architect` members
  - stores entity, faction, learned-tech bitset, Building/Social skill, and chief/architect flag
  - poster selection prefers chief, then architects by exact required-tech coverage, Building skill, Social skill, entity id
- Add `PosterClass::Architect` for build job postings derived from architect-posted blueprints. `JobSource` can remain `Chief` for autonomous public works and `Player` for manual orders.

## Implementation Changes
- Add `Profession::Architect`:
  - primary skill: `SkillKind::Building`
  - job kinds: `JobKind::Build`
  - no new salary system in v1; architects earn through existing build-work postings, and appointment is chief-driven.
  - update profession display, inspector EV table, caps, capital/profession matches, tests, and docs.
- Add `chief_architect_appointment_system`:
  - runs every `TICKS_PER_DAY / 4`, after chief selection and before organic pressure/project selection.
  - candidates are non-chief `Profession::None` adults with at least one Learned construction-relevant tech.
  - target count is `0` if the chief already covers all known construction techs in the faction; otherwise `1`, plus one at 40 and 80 members, capped at `3`.
  - promotes candidates maximizing construction-tech coverage, then Building, then Social; demotes extra architects beyond target with a one-person hysteresis buffer.
- Replace runtime construction adoption gates:
  - `organic_settlement` pressure generation, intent filtering, shelter kind, wall material, bridge emission, and construction-era milestone checks should use poster-pool Learned knowledge.
  - `construction::generate_candidates` should no longer build from `community_adoption_bitset`; runtime candidates must be filtered by `select_poster_for_intent`.
  - A single poster must satisfy every gated piece of a multi-blueprint intent, such as Hut/Longhouse/CompositeHouse. Do not combine different architects’ techs for one building.
  - `spawn_intent`, `plan_building`, `plan_composite_building`, bridge emission, nomad shelter directives, terraform pending footprints, and wall-upgrade rebuilds must propagate the same `posted_by` and `design_techs` to every emitted blueprint.
- Use blueprint design at completion:
  - structure tier helpers (`best_bed_for`, `best_door_for`, `best_hearth_for`, `best_workbench_for`) should read `bp.design_techs`, not current community adoption.
  - completed construction should record recent tech use for the blueprint’s recipe gate so society-wide adoption can still emerge from architect-led practice.
  - existing no-tech builds still post freely.
- Update player manual construction:
  - right-click build menu unlocks a build option if any player-faction chief/architect poster can post it.
  - `PlayerCommand::Build` carries or resolves a poster from that pool; the selected actor remains the personal worker/owner.
  - command execution revalidates the poster before spawning the personal blueprint and snapshots `design_techs`.
- Refactor seeding:
  - replace seed-time `seed_techs: FactionTechs` gate usage with an explicit `SeedConstructionProfile::from_era(options.era)` preserving today’s era outcomes for starting walls, beds, doors, hearths, workbenches, yards, and civics.
  - `seed_starting_buildings_system` must not call `faction_can_build`, `community_adoption_bitset`, or poster selection.
  - remove `seed_prime_tech_adoption_system` from the startup sequence if no other startup system needs it; update related comments/tests to state seeding is era-profile driven.

## Test Plan
- Unit-test poster authorization: Learned passes, Aware-only fails, no-tech recipes pass.
- Autonomous runtime tests:
  - chief Learned tech posts a gated blueprint even when community adoption is below Adopted.
  - chief Aware-only plus no architect posts nothing.
  - architect Learned tech posts for the chief; resulting blueprints carry `posted_by` and `design_techs`.
  - multi-tile Hut/Longhouse/CompositeHouse requires one poster who can post all gated parts.
- Player-command tests:
  - selected worker lacks the tech, faction architect knows it, manual build succeeds with `personal_owner = selected worker` and `posted_by = architect`.
  - no chief/architect knows the tech, menu marks locked and command validation fails.
- Seeding tests:
  - Bronze/Chalcolithic starts seed era-appropriate structures even with all `tech_adoption` stages locked.
  - seed mode emits no blueprints and does not depend on `seed_prime_tech_adoption_system`.
- Regression tests:
  - bridge blueprints, terraform-delayed house blueprints, nomadic yurts, and wall upgrades preserve poster/design metadata.
  - completed architect-posted construction contributes recent tech use.
- Run `cargo test --bin civgame`.

## Assumptions
- “Knowledge” means `PersonKnowledge::has_learned`, not awareness.
- Community adoption remains useful for the tech panel and long-term societal modeling, but it no longer authorizes construction blueprints.
- Civic milestones stay as population/era pacing for runtime growth; any construction-tech side of those checks uses available poster knowledge. Seeded structures still bypass runtime milestones.
- Architects are a v1 appointment role, not a new payroll class.
