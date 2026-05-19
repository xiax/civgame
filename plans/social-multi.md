# Ambient Work-Social Multitasking — SHIPPED

Primary work + secondary ambient social, single-owner work channel. `social_contact.rs`.

## What shipped

- `SecondarySocial { partner: Option<Entity>, mode: Ambient, expires_tick }` — **spawned
  on every Person** (`inactive()`), only ever *mutated* (never insert/removed) so the
  Person archetype never churns (construction/seeding layout is sensitive to Person
  `Query` iteration order — the gotcha below).
- `ambient_social_pairing_system` (ParallelA, after `tick_needs_system`, before
  `goal_update_system`/`opportunistic_interrupt_system`): staggered `PAIRING_RESCAN_CADENCE`
  rescan picks deterministic nearest (`(chebyshev, entity-bits)` min) compatible
  same-root coworker within `SOCIAL_RADIUS`; `PAIRING_WINDOW=400`. Off-cadence clears
  invalid pairings. Unilateral; snapshot→field-mutate apply (no Commands, no aliasing).
- `is_ambient_social_compatible` rejects SOLO/Dormant/Drafted/Combat/Raid/Rescue/
  Migrate/Pack/maintenance/dedicated-Socialize; accepts Gather*/Build/Craft/Farm/Haul/
  Stockpile. `AiState` not gated (avoids gather→deposit Idle-blip unpair).
- Shared gate `is_social_contact` (Socialize OR live SecondarySocial, non-Dormant).

### Consumer wiring (decisions: tighten social_fill #1; full-strength info #2; tier ambient #3; reduced bonding rate)

| Consumer | Ambient |
|---|---|
| `social_fill_system` | yes — **tightened**: socially-active + same-root `Person` neighbors only |
| `awareness_gossip_system` | yes — full strength, all neighbors |
| `cluster_tier_promotion_system` | yes — full strength, all neighbors |
| `wage_gossip_system` | yes — full strength, all neighbors |
| `conversation_memory_system` (affinity) | yes — **two-tier**: dedicated `+5/tick` uncapped; ambient `+1/tick` capped at `AMBIENT_AFFINITY_CAP=40` |
| `tech_teaching_system` (mastery) | **no — deliberate-only** (unchanged) |

Detour suppression: `opportunistic_interrupt_system` skips the `Socialize` challenger
while a live `SecondarySocial` exists; `social_fill` ambient drain keeps `needs.social`
low so `SocialScorer`/legacy cascade don't fire.

## Key gotcha — affinity → cohabitation (why bonding is rate-capped)

`RelationshipMemory` affinity feeds `construction.rs::best_partner_bed_for`
(cohabitation/bed reassignment, thresholds `PARTNER=60`/`REASSIGN=80`). At the dedicated
+5/tick rate, ambient work crews saturate affinity within ~a game-minute and "move in
together" en masse → standalone Bed blueprints at Neolithic (regressed
`onenter_era_seeding::neolithic_runtime_no_paleo_beds_or_excess_hearths`). Root-caused by
rigorous one-variable bisection + DIAG instrumentation (NOT archetype churn, NOT
scheduling, NOT social_fill, NOT material scarcity — verified material-seeding does not
fix it). `relationship_decay` is daily (3600t) → far too weak to bound monotonic
per-tick accrual, so a reduced rate *alone only delays* the spike. Fix: ambient bonding
is slow **and capped at an acquaintance ceiling (40) below the cohabitation thresholds**;
crossing into courtship/cohabitation requires deliberate `Socialize` (also preserves a
real incentive to Socialize). Awareness/wage/tier stay full-strength all-neighbors.

## Tests

`social_contact.rs` unit tests (predicates, is_active strictness); `test_fixture.rs`
`ambient_*` integration tests (pairing stamp/clear/despawn/out-of-range, awareness
all-neighbors, no-ambient-teaching, ambient-bonding-caps-at-acquaintance +
dedicated-reaches-courtship parity, social_fill tightening + control, social-need
drain). Full suite: **807 pass, 0 fail**.

## Docs

`src/simulation/CLAUDE.md` (Ambient work-social subsection + gossip/teaching lines),
root `CLAUDE.md` (ParallelA bullet).
