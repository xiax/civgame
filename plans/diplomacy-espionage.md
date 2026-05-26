# Diplomacy — Spies / Sabotage

## Context

Smart-diplomacy P1's non-omniscience invariant (`DiplomaticContactBook` band-level estimates instead of real partner storage) leaves a deliberate information gap. **Spies** are the player/AI lever to close that gap selectively — at the cost of risk. **Sabotage** is the kinetic counterpart: deniable harm without an open war.

This sits on top of the existing diplomacy stack and breaks one important invariant: the evaluator is forbidden from reading partner `FactionStorage`. Spies introduce a **provisional, decaying** view of partner state that the evaluator *can* read — provided it carries a freshness gate.

## Goals

- A chief / player can dispatch one of their Persons as a `Spy` toward a target faction.
- The spy walks to the target's home/market tile (reusing Trader / Courier `Task::Lead` shape), then "embeds" — periodically (`SPY_REPORT_INTERVAL_TICKS = 5 days`) reports a precise snapshot of the target's `FactionStorage` + `treasury` + `member_count` back to the controlling faction's `DiplomaticContactBook` with `freshness_tick`.
- The evaluator gets an opt-in `ContactRecord::spy_intel: Option<SpyIntel>` whose `freshness_tick` decays over `SPY_STALE_TICKS = 30 days` — beyond that, the band-level estimates take over again.
- A spy can be **detected** by the target with probability tied to target's `martial` + `defensive` axes + chief's investigation skill. On detection: spy is captured (Person → `Captured`), grievance bump on the controlling faction, optional execution → `IncidentKind::Attack`.
- **Sabotage** is an explicit player command on top: `SabotageGranary`, `SabotageMarket` cost the spy's cover (forces detection check at higher rate) but damage a target structure / drain a market.

## Critical files

- `src/simulation/spy.rs` (new) — `Spy { handler_faction: u32, target_faction: u32, embed_phase: SpyPhase, last_report_tick: u64 }`, `SpyPhase::{EnRoute, Embedded, Compromised}`. `dispatch_spy_system` (Economy daily) drives phase transitions. `spy_report_system` (Economy, every `SPY_REPORT_INTERVAL_TICKS`) writes `SpyIntel` to the handler's `DiplomaticContactBook`.
- `src/simulation/diplomatic_contact.rs` — `SpyIntel { food: u32, treasury: f32, member_count: u32, freshness_tick: u64 }` field on `ContactRecord`. `contact_book_update_system` decays + invalidates past `SPY_STALE_TICKS`.
- `src/simulation/diplomatic_evaluator.rs` — `evaluate_proposal_v2` reads `ContactRecord::spy_intel` when present and fresh; falls back to bands otherwise. **Non-omniscience invariant preserved**: spy intel still doesn't reach into live `FactionStorage`; it reads the snapshot the spy planted.
- `src/simulation/diplomacy.rs` — `IncidentKind::{SpyDetected, SpyExecuted, SabotageCommitted}` variants; player commands `DispatchSpy / RecallSpy / SabotageGranary / SabotageMarket`.
- `src/ui/diplomacy_panel.rs` — "Espionage" section: list of active spies, their target, freshness; recall / sabotage buttons.

## Detection model

`detection_chance_per_report(target_martial, target_defensive, spy_charisma) -> f32`:
- Base 0.02 per report (5-day cadence).
- `+ target_martial / 255 * 0.05`
- `+ target_defensive / 255 * 0.04`
- `- spy_charisma / 255 * 0.04`
- Sabotage actions raise next-report base to 0.20.

Detection roll uses deterministic `fastrand` seeded by `(handler, target, calendar.year, day)` so it's reproducible.

## Wire protocol

- `PROTOCOL_VERSION → 8`. Four new `PlayerCommand` variants. `SpyIntel` carries `Serialize/Deserialize` — but **only the snapshot view crosses the wire**, never the live `FactionStorage`.

## Open questions

1. **Should spy reports break the non-omniscience invariant test?** The test forbids the evaluator from importing `FactionStorage`. Spy intel is a *snapshot read at the target's home tile*, written into the contact book by `spy_report_system`. The evaluator still doesn't import `FactionStorage`; the snapshot bridges. Test stays green.
2. **Can a spy be turned (counter-intel)?** Bronze-age realism says yes — interesting follow-up. v1: no.
3. **Counter-intelligence as a structure?** A "Guardhouse" structure could lower detection threshold. v1: defensive culture axis covers it; v2 adds the structure.
4. **Multi-spy stacks?** v1: one spy per (handler, target) pair. Stacking gives diminishing returns.
5. **Sabotage of buildings: does it propagate to other settlements?** v1: only the home settlement. v2: spy-controlled travel between target's settlements.

## Phasing

- **P1**: `Spy` component + `dispatch/embed/report/recall` lifecycle + `SpyIntel` field on contact book + detection check.
- **P2**: Evaluator integration — `evaluate_proposal_v2` reads spy intel preferentially when fresh.
- **P3**: Sabotage commands (granary/market) + detection escalation.
- **P4**: AI-initiated spy dispatch (`martial` + `mercantile` ratio drives appetite).

## Verification

After P1: dispatch spy A→B, wait 5+5 days, confirm A's `ContactRecord` for B carries `spy_intel` with `freshness_tick = current`. After 30+ days, confirm `spy_intel` invalidated and evaluator falls back to bands.
After P2: under spy cover, an `OfferTradePact` proposed to a partner with `StockBand::High` but real `Low` stock should price the trade pact differently than a band-only view.
