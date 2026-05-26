# Diplomacy — Federations / Confederations

## Context

Smart-diplomacy P1+P2+P3 (see `smart-diplomacy.md`) shipped pairwise treaties and reputation. A **federation** sits above pairwise alliance — multiple factions act as a single bloc for defence, trade, and incident propagation. The current `Alliance` treaty already covers two-faction mutual defence; federations extend that to **N-way commitment** with a shared identity (member list, joint policy levers, defection costs).

Real-world analogues: Hanseatic League, Iroquois Confederacy, Delian League. Gameplay payoff: lategame coalitions can stand against a dominant militarist faction without each ally having to negotiate pairwise.

## Goals

- A federation is a stable N-faction bloc with one shared name, a member list, and joint commitments.
- Joining a federation auto-grants `AccessKind::FullTerritory` to every existing member.
- Attacking one member ⇒ war with every member (via `IncidentKind::Attack` propagation).
- Trade incidents (`TradeCompleted`) propagate a fraction of the trust bump to every other member pair — small but real "trade enriches the bloc" effect.
- Leaving the federation costs trust with every member; expulsion is possible by member vote (no UI surface in v1 — just a chief-level mechanic).

## Critical files

- `src/simulation/federation.rs` (new) — `Federation { id, name, members: Vec<u32>, founded_tick, charter: FederationCharter }`, `FederationCharter { mutual_defence: bool, trade_propagation_pct: f32, joint_naming: bool }`, `FederationMap` Resource keyed by `FederationId(u32)` with `by_faction: AHashMap<u32, FederationId>`.
- `src/simulation/diplomacy.rs` — `PlayerCommand::{ProposeFederation, AcceptFederationInvite, LeaveFederation, ExpelFromFederation}` faction-level commands. `IncidentKind::{FederationJoined, FederationLeft, FederationExpelled}` variants.
- `src/simulation/access_grant.rs` — `treaty_to_grant_sync_system` reads `FederationMap.by_faction` and treats co-membership as auto-`FullTerritory` (per the P2 reconciliation shape).
- `src/simulation/raid.rs` / `trespass.rs` — `IncidentKind::Attack` propagates to every federation co-member via `propagate_attack_through_federation(federation_map, ledger, attacker, victim)`. Same shape for `Raid`.
- `src/ui/diplomacy_panel.rs` — Federation section above the faction list: "Your federation: <name> — N members" with per-member quick-jump. Right-click on a faction in the list: "Propose federation".

## Wire protocol

- `PROTOCOL_VERSION → 6`. Four new `PlayerCommand` variants. `Federation` + `FederationCharter` carry `Serialize`/`Deserialize`. Bincode round-trip tests in `net/protocol.rs`.

## Open questions

1. **One federation per faction or many?** Real history says one. Easier model. Lock to one in v1.
2. **Mutual-defence opt-in or compulsory?** v1: compulsory (defines the bloc).
3. **Founder vs members in voting?** v1: any member can leave any time. Expulsion requires founder + majority — but no UI in v1, so AI-only.
4. **How does a war between two members resolve?** Auto-expulsion of the aggressor (no civil war state).
5. **Federation-with-federation diplomacy?** v1: no — federations are leaf nodes. Pairwise diplomacy below the bloc remains unchanged.
6. **Income tax / shared treasury?** v1: no shared treasury. Member factions stay sovereign on currency.

## Phasing

- **P1**: data model + `FederationMap` + `ProposeFederation/AcceptFederationInvite` commands. AI never proposes — player-only in v1. Membership auto-grants FullTerritory.
- **P2**: attack/raid propagation. `IncidentKind::Attack` on one member ⇒ all members declare war on aggressor via `declare_war` per pair.
- **P3**: trade-incident propagation (small per-pair trust bump on every `TradeCompleted` within bloc).
- **P4**: AI-initiated federation proposals using shared-enemy heuristic (two factions both at war with X with high trust → propose federation).
- **P5**: expulsion + LeaveFederation behaviour.

## Verification

After P1: form federation of 3 player+AI factions. Confirm `AccessGrantTable` shows FullTerritory to every member.
After P2: declare war on one member from outside. Confirm `DiplomacyLedger.treaties` flips to War for every other (attacker, member) pair within one tick. Activity log surfaces "Federation defence" entries.
