# Institutional Opportunity Producers

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 2.
**Depends on:** Phase B (scorers exist) and Phase E (`MethodScoringContext` exists) of parent plan.

## Trigger

Pick this up when the third or fourth modern-age domain (Heal, Trade, Teach) is being added and the per-scorer scan pattern is starting to feel ad-hoc. Diminishing returns if landed earlier — most building blocks are already cached, the missing piece is uniform iteration, which only pays off once N > 2 domains use it.

## Scope

Today each scorer/dispatcher independently scans for opportunities (jobs, plots, partners, workshops). Most building blocks are cached (`JobBoard`, `FactionStorage`, `WorkshopOwnership`, `PlotIndex`, `SharedKnowledge`, `GatherClaims`, `StorageTileMap`) but there's no uniform "opportunities I could pursue right now" surface — scorers reach in via different APIs per goal family.

Introduce a producer-driven `OpportunityIndex` so adding a domain = adding one producer + one scorer.

## Current state (from survey)

- Postings produced by: `chief_job_posting_system`, `household_contract_posting_system`, `worker_self_post_stockpile_system`, `esteem_driven_posting_system`.
- Postings consumed by: `earnincome_goal_override_system` + several dispatchers that scan `JobBoard` per claim.
- No producer trait, no central index. Each consumer maintains its own scan logic.

## Files to touch

- New `src/simulation/opportunity.rs`:
  - `enum OpportunityKind { PaidJob, ApprenticeshipSlot, MarketTrade, CareNeed, TeachingSlot, CivicWork, StudyVenue }`.
  - `struct Opportunity { kind, tile: (i32, i32), faction_id, payload: OpportunityPayload, expires_tick }`.
  - `resource OpportunityIndex { by_kind: AHashMap<OpportunityKind, Vec<Opportunity>> }` with `by_kind(kind) -> &[Opportunity]` accessor and bucketed-by-region secondary index.
  - `trait OpportunityProducer { fn produce(&self, world: &World) -> Vec<Opportunity>; fn cadence(&self) -> Cadence; }`.
- `src/simulation/jobs.rs` — `JobBoardOpportunityProducer` adapts `JobBoard` entries into `Opportunity::PaidJob`. Event-driven invalidation on posting added/claimed/expired.
- `src/economy/market.rs` — `MarketTradeOpportunityProducer` surfaces per-settlement price gaps. Daily rebuild.
- `src/simulation/apprenticeship.rs` — `ApprenticeshipOpportunityProducer` lists masters with open mentor slots.
- `src/simulation/goal_scorers.rs` — convert `EarnIncomeScorer` and any new modern-age scorer (Heal, Teach, Trade) to iterate `OpportunityIndex.by_kind(...)` instead of bespoke scans.

## Open questions a real plan must resolve

- **Rebuild cadence.** Per-tick stale-OK index vs event-driven invalidation? Probably event-driven for discrete events (postings — already events), per-day for ambient (market gaps, mentor slots).
- **Distance / reachability.** Each opportunity carries a tile — scorers apply per-agent distance decay, or producer pre-buckets by region? Recommend region-bucket producer + per-agent decay in scorer.
- **Per-faction scoping.** Opportunities visible to other-faction agents only via `SharedKnowledge` gossip, not raw producer output. Producers emit facts; visibility filter sits in scorer.
- **Stale entry eviction.** `expires_tick` field — who runs the eviction sweep, producer or central?
- **Memory footprint.** With 1000s of postings × multiple domains, the index grows. Cap per kind, or compress to summary stats for distant agents?

## Acceptance criteria

- 3+ scorers read from `OpportunityIndex` instead of scanning ECS.
- Adding a new domain (e.g. Heal) requires one producer + one scorer registration — no edits to unrelated systems.
- Job-board behaviour unchanged for existing `EarnIncomeScorer` path; calibration tests on paid-work selection pass.
- Per-tick scan time in profile decreases for systems migrated to the index.
