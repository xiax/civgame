# Knowledge-Based Technology Adoption Overhaul

## Summary

Overhaul technology so no faction “unlocks” tech by decree. Technology lives in people, artifacts, repeated practice, and institutions. `PersonKnowledge` remains the source of mastery; faction/settlement tech becomes a derived adoption state used for planning, UI, bonuses, and autonomous job posting.

The historical model is: invention is rare insight, mastery is trained skill, adoption is social diffusion plus practical use, and institutional technologies require specialists, officials, records, population scale, and durable infrastructure.

## Core Model

- Keep `PersonKnowledge::{aware, learned, study_progress}` as the canonical personal system.
- Replace authoritative `FactionTechs` with a derived `TechAdoptionCache` per settlement/faction.
- Add adoption stages per tech: `Unknown`, `Rumored`, `Demonstrated`, `Practiced`, `Adopted`, `Institutionalized`.
- Treat eras as labels derived from adopted techs, not gates.
- Make discovery grant `Aware` plus partial `study_progress`; it must not directly grant `Learned`.

## Adoption Rules

Add `TechAdoptionDef` metadata beside `TechDef`:

- `Personal`: usable once an individual learns it, such as hunting spear or horseback riding.
- `Household`: spreads through family practice, such as food smoking, pottery, weaving.
- `Subsistence`: requires broad seasonal practice, such as crop cultivation and animal husbandry.
- `Specialist`: requires trained workers, stations, and materials, such as copper working or bronze casting.
- `MilitaryTransport`: requires equipment, animals, trained operators, and repeated deployment.
- `Institutional`: requires officials, population scale, record keeping, civic buildings, or ritual authority.

Stage derivation:

- `Rumored`: at least one local person is aware, or a traded artifact/tablet/book carries the tech.
- `Demonstrated`: local community has observed use, owns a relevant artifact, or has a relevant station/building.
- `Practiced`: at least one learned local practitioner has successfully used the tech recently.
- `Adopted`: thresholds are met for the tech’s adoption scale.
- `Institutionalized`: adopted for one year, or preserved by writing plus living teachers/specialists.

Default thresholds:

- `Personal`: 1 learned practitioner.
- `Household`: max(2 adults, 20% of adults) learned or 50% of households exposed through practitioners/artifacts.
- `Subsistence`: 30% of households participating plus repeated seasonal use.
- `Specialist`: 1 learned specialist, relevant station/materials, and 3 successful uses in 60 days.
- `MilitaryTransport`: 2 learned users or 1 trainer plus required animals/equipment and 3 deployments.
- `Institutional`: chief/bureaucrat/scribe learned, prerequisites adopted, population threshold met, relevant civic process/building present.

## Implementation Changes

- Add `technology_adoption.rs` with adoption-stage structs, threshold helpers, and derivation systems.
- Change `FactionData.techs` into a compatibility cache sourced from adoption, then migrate call sites to explicit helpers:
  - `can_direct_tech(faction, tech)` for chiefs/planners.
  - `community_has_adopted(faction, tech)` for civic/building/bonus gates.
  - `worker_can_perform(person, tech)` for craft/task execution.
- Update recipe/building/job posting:
  - posting can appear at `Practiced` for specialist/household work;
  - civic and settlement-scale construction requires `Adopted`;
  - claiming/completion still requires a worker with `Learned`.
- Update bonuses:
  - personal yield/combat bonuses come from the acting person’s learned techs;
  - storage, settlement, civic, and market bonuses come from adopted or institutionalized techs.
- Update starting-era seeding:
  - common techs learned by many adults;
  - specialist techs learned by a few specialists;
  - institutional techs learned by chiefs/bureaucrats/scribes and marked adopted only when seeded infrastructure exists.

## Historical Behavior

- Technologies can arrive through trade before they are usable.
- A village can know of bronze but fail to adopt it without tin, copper, furnaces, and trained smiths.
- A chief dying no longer erases technology if practitioners and artifacts remain.
- A tiny group can have skilled individuals but cannot institutionalize writing, armies, markets, or monuments without scale.
- Knowledge can decay from `Institutionalized` to `Adopted` or `Practiced` if all teachers, artifacts, and use disappear for long enough.

## UI And Docs

- Tech panel should show adoption stage, learned/aware counts, practitioner count, and missing adoption requirements.
- Inspector keeps personal learned/aware display.
- Activity log distinguishes `Insight`, `Learned`, `Practiced`, `Adopted`, and `Institutionalized`.
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` to document the new model.

## Test Plan

- Unit-test stage derivation for each adoption scale.
- Verify discovery only grants awareness/progress.
- Verify passive teaching adds progress rather than instant learned mastery.
- Verify workers cannot claim gated craft jobs unless personally learned.
- Verify community bonuses do not activate from one chief merely being aware.
- Verify chief succession preserves adopted tech when practitioners remain.
- Verify newborns inherit awareness only.
- Integration-test one tech moving from insight to learned practitioner to practiced to adopted.

## Assumptions

- No new crates.
- Keep the existing 44-tech `TechId` tree.
- Keep bitsets for personal knowledge.
- Put the fleshed-out written plan at `plans/knowledge_based_technology_adoption.md` when execution is enabled.
