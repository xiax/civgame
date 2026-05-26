# Smart Diplomacy AI

## Context

`simulation/diplomacy.rs` already ships the ledger, reputation tracks, four treaty types, six proposal variants, daily decay, raid integration, trespass / defense queue, and a player diplomacy panel. What's missing is the *brain*: today's `ai_diplomacy_proposal_system` reads four scalar thresholds (trust ≥ 0 → trade, trust ≥ 50 + familiarity → alliance, fear ≥ 60 → peace) and ignores `FactionCulture` (already rolled with `martial / mercantile / defensive / ceremonial` axes), known stocks, distance, war fatigue, and any notion of fairness. `DemandTribute` accepts under fear but has no side effect; `OfferAid` is a familiarity bump with no goods moving. Trespass is binary (treaty → allowed, no treaty → warning), so a trader and a war band hit the same classifier.

This plan turns that scalar-threshold AI into a utility-driven engine that perceives only what its faction could plausibly know, scores deal value across economic / security / relationship / strategic / risk axes, predicts the other side's acceptance, and follows through with physical courier delivery — shipped in three phases so each lands green.

## Goals

- AI proposes the *right* deals for its culture and situation, not random rep-threshold flips.
- Factions only negotiate with factions they've actually contacted (scouts, traders, visited settlements, gossip, trespass, treaty history).
- Accepted resource/currency transfers physically move via couriers; failure has reputational consequences.
- Trespass classification depends on the intruder's *role* (trader, nomad, soldier), not only treaty state.
- Player-facing surface (proposal cards, accept/reject, panel labels) stays clear: AI's deal labels (`Fair`, `Generous`, `HardBargain`, `Exploitative`) are visible.

## Phase 1 — Utility evaluator + non-omniscient contact book (single-term deals only) — **SHIPPED 2026-05-26**

P1 keeps the existing 6 `DiplomacyProposal` variants on the wire (no protocol bump) and replaces the brain.

### Contact book (`simulation/diplomatic_contact.rs`, new)

- `DiplomaticContactBook` resource: `AHashMap<u32 /* viewer root faction */, FactionContacts>`.
- `FactionContacts { known: AHashMap<u32 /* target root */, ContactRecord>, last_recomputed_tick: u64 }`.
- `ContactRecord { first_contact_tick, last_contact_tick, contact_sources: ContactSourceSet, known_home_tile: Option<(i32, i32)>, known_market_tiles: SmallVec<[(i32,i32); 4]>, last_known_member_count_band: PopBand, last_known_food_band: StockBand, last_known_military_band: MilitaryBand, route_reachable: bool }`.
- `ContactSourceSet` = bitset over `{VisitedSettlement, TraderTrip, ScoutSighting, GossipFromAlly, TrespassOnUs, IncomingProposal, Materialization, RaidedUs}`.
- `Band` enums are coarse buckets (`Unknown / Low / Medium / High`) so AI estimates aren't omniscient. Sources of each band:
  - `PopBand`: from `AgentMemory.visited_settlements` LRU + cohort-summary peek when materialised.
  - `StockBand`: from observed market price (high price → Low stock) + `IncidentKind::Aid` / `TradeCompleted` totals.
  - `MilitaryBand`: from observed `IncidentKind::{Attack, Raid}` + sightings of armed members on our territory.
- `contact_book_update_system` (Economy, every `TICKS_PER_DAY`):
  - Walks `AgentMemory.visited_settlements` (already populated) for every player-faction member → folds into viewer's `FactionContacts`.
  - Reads recent `IncidentKind::*` from `DiplomacyLedger.incident_log` (last 16 entries already cached) → bumps `Trespass/Raid/Trade/Aid` sources.
  - Reads `MemoryKind::HostileFactionSighting` clusters from `SharedKnowledge::Faction(fid)` → `ScoutSighting` source + populates `known_home_tile` (cluster centre).
  - Reads `AbstractFactions` for materialised pairs to seed `Materialization` when first met.
- `is_known(viewer_root, target_root) -> bool`: true iff `contact_sources != 0`.

### DiplomaticPersonality (pure projection)

`DiplomaticPersonality::from_culture(&FactionCulture, &FactionData)` returns a stack-allocated bag of weights:

```
{ peace_threshold_bias, tribute_aggression, alliance_appetite, trade_appetite,
  border_tolerance, trespass_warn_grace, fairness_floor (HardBargain ↔ Fair),
  min_proposer_gain, min_predicted_acceptance_gain }
```

Derived from existing 4 culture axes (no new state):
- `martial` raises `tribute_aggression`, lowers `fairness_floor` (HardBargain ok), lowers `min_proposer_gain` (riskier offers).
- `mercantile` raises `trade_appetite`, raises `fairness_floor` (prefers Fair+).
- `defensive` raises `border_tolerance` (more grace before warning), lowers `alliance_appetite`.
- `ceremonial` raises `alliance_appetite`, raises long-trust weighting in evaluator.
- Lifestyle/density (already on `FactionData.caps`) influence `border_tolerance` for nomads vs settled.

Re-derived every tick (cheap, no drift with `drift_culture`).

### DealEvaluator (pure-fn)

`evaluate_proposal_v2(proposal: DiplomacyProposal, viewer_fid: u32, partner_fid: u32, viewer_role: ProposerOrReceiver, ledger, contact_book, registry, market_view) -> DealUtility`.

`DealUtility { economic: f32, security: f32, relationship: f32, strategic: f32, execution_risk: f32, net: f32, fairness: FairnessLabel }`.

Axes:
- `economic`: resource/currency only. For `OfferAid { qty }` proposer side = `-qty × trade_base_value × surplus_discount(viewer.storage, rid)`; receiver side = `+qty × trade_base_value × scarcity_mult(viewer.storage, rid)`. `OfferTradePact` priced from observed price gap × estimated volume / year.
- `security`: NAP / Alliance / Peace add `border_tolerance × distance_pressure`. War or grievance ≥ 60 raises peace gain. Reads `TerritoryMap.contested_tiles` against the pair.
- `relationship`: `trust + 0.5 × familiarity − 0.5 × grievance`, scaled by ceremonial/trade appetite.
- `strategic`: shared enemy (any third faction at War with both per ledger) lifts NAP/Alliance. Trade route viability uses `route_reachable` flag from contact book.
- `execution_risk`: chebyshev distance to partner home / `trade_base_value` × 0.01; uncertainty if `contact_book.last_contact_tick` stale (> 30 days).
- `net = economic + security + relationship + strategic − execution_risk`.

`FairnessLabel`:
- `fairness_ratio = max(receiver_economic, 0) / max(proposer_ask, 1e-3)` (clamped). Pure-gift (`proposer_ask == 0`) → `Generous`. `Fair` ∈ [0.85, 1.15], `HardBargain` ∈ [0.4, 0.85], `Exploitative` < 0.4. Tribute under fear stays `Exploitative` but is sendable under martial personality.

Pure-fn keeps it unit-testable without an `App`.

### Hard blocks (acceptance gate)

`acceptance_blocked(proposal, viewer_fid, partner_fid, ledger, registry, contact_book, storage) -> Option<BlockReason>`:

- `Self` / same root.
- `Unknown` — `contact_book.is_known` false for receiver perspective ⇒ block silently (proposer can still send if they know us; we just can't formulate an informed response and reject).
- `WarTreatyConflict` — anything except `OfferPeace` blocked while at war.
- `AllianceConflictWithSharedEnemy` — Alliance proposal blocked when proposer is at war with our ally (loyalty).
- `ImpossibleDelivery` — for transfer terms in P3; P1 just checks `OfferAid { qty }` against proposer's storage.
- `ExpiredProposal` — `posted_tick + PROPOSAL_EXPIRY_TICKS` past `now`.

### AI decision loop (replaces `ai_diplomacy_proposal_system` body)

Daily-quarter cadence (already throttled by `(day + faction_id) % 5`). For each (viewer, target) known pair:

1. Skip if `contact_book.is_known` false or `cooldown_until_tick` set.
2. Build a candidate set from motives: `OfferPeace` (if at war), `OfferTradePact`, `OfferAlliance`, `OfferNonAggression`, `DemandTribute`, `OfferAid`.
3. For each candidate: run `evaluate_proposal_v2` for viewer (proposer side) and the *predicted* receiver side using `contact_book` estimates (not real receiver storage).
4. Keep candidates where `viewer.net ≥ personality.min_proposer_gain` AND `predicted_receiver.net ≥ personality.min_predicted_acceptance_gain`.
5. Apply personality fairness gate: if `fairness == Exploitative` and personality is not martial-dominant, drop.
6. Argmax by `viewer.net`. Post via existing `ledger.post_proposal(...)`.
7. Stamp `OfferMemory { pair, proposal_fingerprint, posted_tick, predicted_gap }` (new field on `DiplomaticRelation`, ring length 4) so the same shape isn't re-sent inside `OFFER_RESEND_COOLDOWN = 5 days`. Bias, not exclusion — old entries score-penalise, not block.

Receiver path (`ai_diplomacy_response_system`) replaces `evaluate_proposal` call with: rebuild `DealUtility` for receiver against the real ledger (proposer-side estimates not needed — receiver knows itself), then `Accept` iff `net ≥ personality.min_predicted_acceptance_gain` AND no hard block. Keeps `apply_accepted_proposal` unchanged.

### DemandTribute actually does something (P1 follow-on)

Accepted `DemandTribute` now calls `FactionRegistry::set_dominance(proposer, receiver)` (already exists per ledger CLAUDE.md). The existing `tribute_payment_system` (daily, transfers `TRIBUTE_PER_DAY` from subordinate → dominant treasury) starts flowing automatically. Add `IncidentKind::TributeAccepted` for the activity log.

### Critical files (P1)

- `src/simulation/diplomatic_contact.rs` (new) — book + update system + `is_known` queries.
- `src/simulation/diplomatic_personality.rs` (new) — pure-fn projection from culture.
- `src/simulation/diplomatic_evaluator.rs` (new) — `DealUtility`, `evaluate_proposal_v2`, `acceptance_blocked`, `FairnessLabel`. Pure module, no Bevy imports.
- `src/simulation/diplomacy.rs` — replace bodies of `ai_diplomacy_proposal_system` + `ai_diplomacy_response_system`. Add `OfferMemory` ring to `DiplomaticRelation`. Add `IncidentKind::TributeAccepted`. Keep `evaluate_proposal` as thin shim delegating to evaluator (callers in fixtures).
- `src/simulation/SimulationPlugin` — register contact-book update system + resource.
- `src/ui/diplomacy_panel.rs` — gate left-column list on `contact_book.is_known(self, target)` so unknown factions don't appear. Show `FairnessLabel` next to incoming proposals.

### Tests (P1)

Pure unit tests in evaluator module (no `App`):
- Hungry mercantile faction proposes Trade or RequestAid (via `OfferAid` with negative qty stand-in), never Alliance.
- Strong martial faction generates `DemandTribute` only when `MilitaryBand(target) < MilitaryBand(self)` AND `fear ≥ 80`.
- Defensive faction declines NAP when grievance high.
- Receiver accepts peace under fear ≥ 60 OR sustained war fatigue (low `last_known_food_band`).
- `is_known` blocks AI from proposing to never-contacted factions.
- `OfferMemory` ring penalises same fingerprint within 5 days, allows after.

Behavioural test via `test_fixture`:
- Two factions; inject `MemoryKind::HostileFactionSighting` for one; tick a week; assert proposal arrives only after contact source recorded.

## Phase 2 — Directional access grants + intent-aware trespass — **SHIPPED 2026-05-26**

P2 adds finer trespass semantics without touching the deal model.

### AccessGrant resource

`AccessGrantTable` (Resource): `AHashMap<(grantor_fid, grantee_fid), Vec<AccessGrant>>`.

`AccessGrant { kind: AccessKind, expires_tick: Option<u64> }` where:
- `AccessKind::MarketCorridor { settlement_id, radius: u8 }` — trader/courier within disc around `market_tile`.
- `AccessKind::SeasonalCamp { disc_center: (i32,i32), radius: u8, season_window: SeasonSet }` — nomad camp area.
- `AccessKind::SafePassage { until_tick }` — civilian transit, no harvest/build, no military.
- `AccessKind::FullTerritory` — current `Alliance` semantics; auto-granted on Alliance.

`TradePact` no longer implicitly grants `FullTerritory`; it auto-creates `MarketCorridor { radius: 6 }` around each of grantor's `Settlement.market_tile`s.

### Intent classification

`intruder_intent(entity, &Person, equipment, &PersonAI, &FactionData) -> IntruderIntent`:
- `Drafted` or `JobClaim::RaidParty` member → `Hostile` regardless of access.
- `Profession::Trader` carrying tradeable goods → `CivilianTrader`.
- Nomadic faction `Person` outside own territory → `Nomad`.
- Otherwise `Civilian`.

`trespass_detection_system` calls `intruder_intent` then `access_grant_table.permits(grantor, grantee, intent, tile)`:
- `Hostile` always trespasses regardless of grants.
- `CivilianTrader` permitted in `MarketCorridor`.
- `Nomad` permitted in `SeasonalCamp` if current season ∈ window.
- `Civilian` permitted by `SafePassage` (any tile) or `FullTerritory`.

### Personality-aware warning policy

`trespass_handling_system` reads grantee's `DiplomaticPersonality`:
- `defensive` / `martial` → warn on first incident, escalate after 2 (current default lowered by 1).
- `mercantile` → tolerate trader-intent for one extra grace incident even outside corridor.
- High trust (`> 40`) grants one extra grace incident across the board.

### Player surface

- Diplomacy panel right pane gets "Grants" section listing live `AccessGrant`s in both directions; player can revoke their own grants (faction-level `RevokeAccessGrant` command). Auto-created `MarketCorridor` on trade-pact-accept shows up immediately.

### Critical files (P2)

- `src/simulation/access_grant.rs` (new) — table + `permits(...)` pure-fn + `apply_accepted_proposal` extension that auto-creates `MarketCorridor` on TradePact accept.
- `src/simulation/trespass.rs` — replace flat `is_trespass` with grant-table + intent-classifier path. Keep `is_trespass` for tests as shim over `Hostile`-only check.
- `src/simulation/diplomacy.rs` — `PlayerCommand::RevokeAccessGrant { faction_id, target_faction_id, kind }` (new variant; serde derive; net protocol bump).
- `src/net/protocol.rs` — `PROTOCOL_VERSION = 4`, add `AccessGrant` serde, bincode round-trip tests.
- `src/ui/diplomacy_panel.rs` — Grants section + Revoke buttons.

### Tests (P2)

- TradePact accept auto-creates `MarketCorridor`; civilian trader in corridor does not trigger trespass; soldier in corridor does.
- Nomad in `SeasonalCamp` permitted in summer, blocked in winter when window narrowed.
- Revoke command drops grant; next tick same actor trespasses.
- Defensive personality warns on first incident, neutral personality on second.

## Phase 3 — Multi-term DealPackage + courier obligations + concession ladder — **SHIPPED 2026-05-26**

P3 adds the compound-deal abstraction and physical follow-through. Wire-bumped protocol.

### DealPackage / DealTerm

```
DealPackage { id: DealId, from: u32, to: u32, terms: SmallVec<[DealTerm; 4]>, posted_tick, expires_tick }
DealTerm ∈ {
  TreatyForm(TreatyKind),
  TreatyBreak(TreatyKind),
  ResourceTransfer { resource_id, qty, direction: FromOrTo },
  CurrencyTransfer { amount, direction },
  AccessGrantTerm { grant: AccessGrant, direction },
  TributeStream { until_tick, daily_units, direction },
}
```

Backward compatibility: existing 6 `DiplomacyProposal` variants stay on the wire as single-term sugar (`OfferAid { qty } → ResourceTransfer { food_id, qty, ToReceiver }`). Receiver always sees `DealPackage`. Player-issued commands grow `SendDiplomacyDealPackage { faction_id, target_faction_id, terms }` alongside legacy `SendDiplomacyProposal`.

### Concession ladder + OfferMemory bias

`OfferMemory.predicted_gap` is the receiver's predicted `net` deficit at proposal time. On rejection:
- `gap < 0.1 × min_acceptance` → retry after `RETRY_COOLDOWN_TICKS = 1 day` with one concession step (add 5 currency, shorten access duration, drop alliance to NAP, swap tribute to resource exchange).
- `gap < 0.5 × min_acceptance` → `LONG_COOLDOWN_TICKS = 5 days`, shift motive (alliance → NAP, tribute → trade).
- `gap ≥ 0.5` → `SHIFT_MOTIVE_COOLDOWN = 10 days` + small grievance bump if repeated tribute/aid demand.

Per feedback memory "PlanHistory should bias, not exclude": `OfferMemory` is a score penalty, never an absolute block.

### Couriers (durable obligations)

Accepted package with any transfer term creates one or more `DealObligation` entities:

```rust
#[derive(Component)]
struct DealObligation {
    deal_id: DealId,
    from_faction: u32,
    to_faction: u32,
    term: DealTerm,   // single transfer term, expanded per term
    deadline_tick: u64,
    status: ObligationStatus, // Pending | InTransit { courier: Entity } | Delivered | Defaulted
}
```

`courier_assignment_system` (Economy, daily):
- For each `Pending` obligation, picks an eligible carrier from grantor faction:
  - Resource/currency transfer → `Profession::Trader` first, then `Bureaucrat`, then idle adult with `Carrier` slot.
- Pulls payload from faction storage via existing `WithdrawMaterial` shape (reuses `chief_post_funding_system`-style escrow if it's a paid term).
- Posts `JobKind::Diplomatic` (new variant) onto the JobBoard so the chosen agent picks it through normal `job_claim_system`. Stamps obligation `InTransit { courier }`.

`htn_diplomatic_courier_dispatch_system` (ParallelB): for `JobClaim::Diplomatic` holders not at destination, routes `Task::Lead { dest = target.market_tile }` (same primitive trader uses). On arrival, fires `DeliverDealPayload` task → calls `pay(...)` or storage deposit transactions atomically, then closes obligation (`Delivered`).

`obligation_deadline_system` (Economy, daily):
- Past `deadline_tick` without `Delivered` → status `Defaulted`, record `IncidentKind::DefaultedDeal { deal_id }` (new variant: trust −15, grievance +8), refund any held escrow, retire courier `JobClaim`.

### Hard blocks extended for P3

`acceptance_blocked` adds:
- `ImpossibleDelivery` — proposer's storage / treasury cannot cover all transfer terms at acceptance time (re-check; storage might have drained between propose and accept).
- `AccessRequestedByHostile` — `AccessGrantTerm` blocked while raid party member or drafted military in proposer's roster.
- `TreatyConflict` — `TreatyForm(Alliance)` while either side has Alliance with a faction the other is at war with.

### UI

Diplomacy panel proposal cards expand to render multi-term packages: each term gets a row with proposer-side / receiver-side labels and the AI's internal `FairnessLabel`. Accept/Reject is per-package, not per-term. Activity log gets `DealAccepted { deal_id }`, `DealDelivered { deal_id, resource_id, qty }`, `DealDefaulted { deal_id }`.

### Critical files (P3)

- `src/simulation/diplomacy.rs` — `DealPackage`, `DealTerm`, `DealId`, `OfferMemory` ring extension, `IncidentKind::{DefaultedDeal, TributeAccepted}`.
- `src/simulation/deal_obligation.rs` (new) — entity, courier assignment, dispatch, deadline systems.
- `src/simulation/diplomatic_evaluator.rs` — extend `evaluate_proposal_v2` to walk `DealTerm`s and sum axes.
- `src/simulation/jobs.rs` — `JobKind::Diplomatic` + `JobProgress::Deliver`.
- `src/simulation/htn.rs` — `htn_diplomatic_courier_dispatch_system`.
- `src/net/protocol.rs` — `PROTOCOL_VERSION = 5`, `DealPackage` + `DealTerm` serde + bincode round-trip tests.
- `src/ui/diplomacy_panel.rs` — multi-term card rendering, fairness label colour key, deal-status tracker.

### Tests (P3)

- Fair single-term resource exchange: accepted, courier dispatched, on arrival storage deltas match terms, trust delta recorded.
- Failed delivery (courier dies en route): obligation defaults at deadline, `DefaultedDeal` incident logged, trust drops.
- Concession ladder: rejected near-fair offer retried 1 day later with a sweetener; far-fair offer not retried at all.
- Multi-term package (NAP + 10 grain → 5 currency): all terms commit on accept; rolling back one term fails the whole package atomically.
- Player accepting AI proposal in panel produces same side effects as AI-AI acceptance.

## Cross-phase concerns

- **Non-omniscience invariant:** `evaluate_proposal_v2` only takes `viewer_fid` + `contact_book` + ledger. Forbidden from reading partner's `FactionStorage`, `EconomicAgent.currency`, or `PersonKnowledge` directly. Receiver-side path reads its own storage freely. Unit test asserts the evaluator module doesn't import `FactionStorage`.
- **Net protocol versioning:** P1 stays at `PROTOCOL_VERSION = 3`. P2 bumps to 4 (adds `RevokeAccessGrant` + `AccessGrant` serde). P3 bumps to 5 (adds `DealPackage`, `DealTerm`, new incident variants). Each bump carries fresh bincode round-trip tests in `net/protocol.rs`.
- **Abstract factions:** `materialized = false` factions are valid diplomacy *targets* (player sees them in panel if contact recorded) but don't run AI proposer logic — `ai_diplomacy_proposal_system` keeps its `data.materialized` filter. P2 access grants and P3 couriers no-op against abstract factions; trespass and grants activate on materialization.
- **Goal/HTN integration:** Adding `AgentGoal::DiplomaticCourier` would mean a new goal scorer + dispatcher and breaks the "no-task backstop" invariant; instead P3 routes couriers through `AgentGoal::Earn` + `JobKind::Diplomatic` so existing job claim / payout machinery applies (couriers get paid from the proposer's treasury via `chief_post_funding_system`-shape escrow).
- **CLAUDE.md updates:** Each phase updates `src/simulation/CLAUDE.md` Diplomacy & Territory section + `src/ui/CLAUDE.md` Diplomacy panel section.

## Verification

After each phase:

- `cargo test --bin civgame` (unit + behavioural).
- `cargo run` with two seeded factions in proximity:
  - **P1**: observe activity log for `DiplomacyProposalReceived` and confirm proposal motive matches culture (martial faction with stronger military demands tribute; mercantile faction offers trade). Open diplomacy panel; reject offers and confirm cooldown via `OfferMemory`.
  - **P2**: form trade pact; send a Trader through partner territory and confirm no trespass warning. Send a drafted Hunter through and confirm immediate warning + grievance.
  - **P3**: accept a 10-grain aid deal from AI; watch courier walk physical grain to our market_tile; check storage delta. Block courier path (deconstruct bridge); confirm deadline default fires `DealDefaulted` and trust drops.

## Carryovers — shipped 2026-05-26

- **Concession ladder behaviour** wired on top of `OfferMemory.predicted_gap`: `cooldown_ticks_for_gap` returns 1d / 5d / 10d by gap ratio; `last_offer_near_fair` flips a sweetened multi-term DealPackage retry (treaty + 3 grain) on near-fair rejection. AI receiver now drains the package channel via `ai_diplomacy_package_response_system` — full multi-term send + receive + courier loop.
- **AI-initiated `RevokeAccessGrant`** on grievance crossing `GRIEVANCE_AUTO_REVOKE_THRESHOLD = 40` (= existing `GRIEVANCE_BLOCK_TRADE`). v1 scope: `SeasonalCamp` grants only (treaty-derived grants stay on the War/BreakTreaty channel).
- **Skeleton follow-up plan files** dropped per project memory rule: `plans/diplomacy-federations.md`, `plans/diplomacy-marriage.md`, `plans/diplomacy-espionage.md` — each carries Context / Goals / Critical files / Wire protocol / Open questions / Phasing / Verification.

## Deferred (post-carryover)

- Multi-resource auction-style market-access tariffs.
- Federation / confederation as super-treaty above Alliance → `plans/diplomacy-federations.md`.
- Diplomatic marriages tying `RelationshipMemory` cross-faction → `plans/diplomacy-marriage.md`.
- Spy / sabotage actions (currently outside non-omniscience scope) → `plans/diplomacy-espionage.md`.
- Courier durability under combat beyond carrier-despawn → Pending fallback.
