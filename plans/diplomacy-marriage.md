# Diplomacy — Cross-Faction Marriages

## Context

`RelationshipMemory` (`src/simulation/memory.rs` adjacent) already tracks per-pair affinity within a faction; bonded couples drive `Pregnancy` via `bonding_system`. Today bonds form only between same-root-faction members. Diplomatic marriages would let a chief offer a kinship tie across factions — bumping trust + producing an heir who inherits dual-faction relationships.

Real-world analogues: Habsburg dynastic marriages, Bronze Age royal exchanges. Gameplay payoff: a non-military pathway to durable alliance + slow cultural exchange (the spouse carries their birth-faction's `PersonKnowledge.learned` into the in-law household).

## Goals

- A chief can propose a `Marriage` to a peer chief. Both sides nominate one unbonded adult member.
- On accept, the lower-trust side's nominee migrates to the higher-trust faction's home tile, becoming a `HouseholdMember` of a new shared household.
- The pair bonds at `SPOUSE_AFFINITY = 79` (existing kin constant).
- Trust bumps `MARRIAGE_TRUST_BUMP = 30` between the two root factions.
- Marriage **persists**: a `DiplomaticMarriage` resource tracks the pair; their death/divorce produces an `IncidentKind::MarriageEnded` and modest trust drop.

## Critical files

- `src/simulation/marriage.rs` (new) — `DiplomaticMarriage { spouse_a: Entity, spouse_b: Entity, faction_a: u32, faction_b: u32, formed_tick }`, `DiplomaticMarriageRegistry` Resource. Helper `find_marriage_candidate(&FactionRegistry, &RelationshipMemory, faction_id) -> Option<Entity>` picks oldest unbonded adult.
- `src/simulation/diplomacy.rs` — `PlayerCommand::{ProposeMarriage { faction_id, target_faction_id }, RespondMarriage { faction_id, marriage_proposal_id, response, our_nominee: Option<Entity> }}` (chief-level). `IncidentKind::{MarriageFormed, MarriageEnded}`.
- `src/simulation/diplomatic_evaluator.rs` — `evaluate_marriage_proposal(...)` axis weights (relationship-heavy, security-modest, no economic). `DiplomaticPersonality::marriage_appetite` derived from `ceremonial + (255 - martial)`.
- `src/simulation/reproduction.rs` — newborn from a `DiplomaticMarriage` couple gets `dual_faction_origin: Option<u32>` so their adult relationships seed across both factions.
- `src/simulation/faction.rs` — `FactionRegistry::spawn_household` already exists; marriage path uses it with `parent_faction = Some(higher_trust_faction)`.

## Wire protocol

- `PROTOCOL_VERSION → 7`. Two new `PlayerCommand` variants. `IncidentKind` extension already accommodates new variants (bincode adds discriminants).

## Open questions

1. **Same-sex marriages?** Affinity / household machinery doesn't gate on sex except for procreation. Sim should allow but flag (`Pregnancy` won't trigger). Plan: allow, with explicit "no heir" pathway.
2. **Polygamy?** Existing `Spouse` shape is 1:1. v1: monogamy only; future could lift the cap.
3. **Which faction does the spouse migrate to?** Higher trust receives — encodes "the more powerful party hosts." Player can override on Respond by picking nominee.
4. **What if the spouse dies?** Trust drops `MARRIAGE_DEATH_TRUST_PENALTY = 5` (small — natural death isn't a betrayal). If killed by surviving-faction action: full `MarriageEnded` penalty + grievance bump.
5. **Heir as treaty enforcer?** Bronze-age realism says yes (heir's existence stabilizes alliance). v2: `IncidentKind::HeirBorn` with bonus trust per heir.
6. **Reject path?** Chief who rejects a marriage offer takes no rep penalty (proposal-spam guards already in `OfferMemory`).

## Phasing

- **P1**: `DiplomaticMarriage` Resource + `Propose/RespondMarriage` commands + atomic side-effects (migration, household form, trust bump).
- **P2**: Cross-faction `RelationshipMemory` seeding on newborn (mom + dad's faction adults become known acquaintances at adulthood).
- **P3**: Death/divorce `IncidentKind::MarriageEnded` + matching penalties. Divorce as a player command.
- **P4**: AI-initiated marriages from `ceremonial`-high chiefs. Same evaluator pipeline as other proposals.

## Verification

After P1: two factions, propose marriage from A. Accept. Confirm: one spouse from B migrated to A's home tile, bonded as `Spouse` at affinity 79, trust between root A/B bumped by 30, `IncidentKind::MarriageFormed` in both ledgers.
After P2: wait for offspring. Confirm child's `RelationshipMemory` has positive seeds for grandparents in both factions.
