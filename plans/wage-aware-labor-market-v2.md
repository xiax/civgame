# Wage-Aware Labor Market — v2

Comprehensive replacement for `plans/wage-aware-labor-market.md`, after auditing every claim against the current tree.

## Progress (2026-05-12)

- **Phase 0 (wage payout) — shipped.** `JobEscrowIndex` resource links each paid `JobId` → escrow entity; `JobCompletedEvent` carries `claimants` + `completed`; `job_payout_system` (Economy, exclusive `&mut World`, after `job_claim_release_system`) splits the escrow `amount` across claimants by direct credit (no double-debit of beneficiary), despawns the sidecar, and writes an `Earnings` ring + `ActivityEntryKind::WagePaid` per worker. Currency invariant preserved end-to-end (5 new acceptance tests; 508 total pass).
- **Phase 1 (skill peaks + decay) — shipped.** `Skills` is now clamped at `SKILL_MAX=255`. New components `SkillPeaks`, `SkillUseTicks`, `SkillsLastSeen` on every Person spawn site. `skill_peaks_tracker_system` (observer over `Changed<Skills>`) ratchets peaks + stamps `use_ticks`. `skill_decay_system` runs daily, decays unused skills exponentially (`half-life = 90 days`) toward `SKILL_MASTERED_FLOOR (30)` when `peak ≥ SKILL_MASTERY_LINE (80)`, else `max(SKILL_FLOOR_BASE (5), peak * 0.30)`. No existing `gain_xp` call site needed to change — the observer pattern handles every write path centrally.
- **Phase 4a (chief-funded postings) — shipped.** New `chief_post_funding_system` (Economy, exclusive `&mut World`, after `chief_job_posting_system`) scans the `JobBoard` for newly-posted chief contracts with `reward == 0.0` and, for factions with a non-empty `economic_policy` map (Mixed/Market), debits the faction `treasury` by `chief_wage_for(progress)` and spawns the `JobEscrow { amount, beneficiary: chief_entity }` indexed via `JobEscrowIndex`. `chief_wage_for` keys per-kind: `Stockpile{rid} = trade_base_value(rid) * qty * CHIEF_MARGIN (0.5)`; `Haul = same * 0.5` (transport discount); `Crafting = recipe.output_resource.trade_base_value() * qty * CHIEF_MARGIN`; `Calories / Planting / Building` flat per-day formulas (`CHIEF_FOOD_WAGE_PER_DAY = 2.5`, `CHIEF_FARM_WAGE_PER_DAY = 2.0`, `CHIEF_BUILD_WAGE_PER_DAY = 3.0`, all bounded `≤ 40` / `CHIEF_BUILD_WAGE_CAP = 30`). Unaffordable wage → posting stays at `reward = 0` (no escrow spawned). Subsistence factions (empty policy map) skip funding entirely — chief allocates communal labor exactly as before. No existing reward-0 producer needs editing; the funding system runs as a downstream pass. 516 tests pass (512 prior + 4 Phase 4a).

- **Phase 3 (earnings aggregation + wage signal + gossip) — shipped.** `EarningEntry` and `JobCompletedEvent` now carry `target_rid: Option<ResourceId>`; every completion emission site populates it via `JobProgress::target_rid()` (Calories/Build → None; Stockpile/Haul/Planting/Crafting → the named resource). `FactionData.wage_signal: AHashMap<(JobKind, Option<ResourceId>), WageEMA>` lives on every faction. `faction_wage_signal_system` (Economy, daily) walks every member's `Earnings` ring, sums last-day payouts per `(village, kind, rid)` triple, then folds `ema_per_day` via `α = 1 − 0.5^(1/5) ≈ 0.129` (5-day half-life); fresh keys jump straight to the sample; pre-existing keys with no fresh sample decay toward zero. `PerceivedFactionWages` component (cap 32, oldest-first eviction) captures cross-faction information friction; `wage_gossip_system` (Economy) piggybacks Socialize within `WAGE_GOSSIP_RADIUS = 3` and merges up to `WAGE_GOSSIP_TOP_K = 4` of each socialiser's recent observations (own-faction `wage_signal` + previously-cached `PerceivedFactionWages`), skipping the listener's own faction and applying an `exp(−age / TICKS_PER_DAY)` staleness penalty. 512 tests pass (510 prior + 2 Phase 3).

- **Phase 2 (capital recognition — minimal core) — shipped.** New `src/simulation/capital.rs` module: `OwnedBy { faction_id, kind, tile }` component + `WorkshopOwnership` resource (faction → workshops list) maintained by `on_owned_by_add` / `on_owned_by_remove` hooks registered in `SimulationPlugin::build`. `OwnedBy` is stamped at both workshop finalize sites in `construction.rs` (blueprint finalize + seeded-structure finalize) for Workbench / Loom / Granary / Shrine / Market / Barracks / Monument. `WorkshopKind::affine_to(Profession)` maps Market → Bureaucrat today; Workbench / Loom / Shrine wait on Phase 5's `Crafter` / `Healer` variants. Three read-only capital-factor helpers (`tool_capital_factor`, `workshop_capital_factor`, `land_capital_factor`) plus a composite `capital_factor(...)` Phase 4 will consume. `tool_profession` maps `weapon → Hunter` today via `core_ids`; the catalog split (Hoe / Awl / Loom shuttle / Hammer / Bow with `prof:*` tags, plus tool durability) is deferred. `land_capital_factor` reads `PlotIndex` for any Agricultural plot held under non-StateOwned tenure by the agent's `HouseholdMember`. 510 tests pass (508 prior + 2 capital).
- **Phase 4b (EV-aware ranking + scaffolding) — shipped.** New `src/simulation/profession_choice.rs` module: pure helpers (`skill_competence` clamped to `[0.2, 1.0]`; `job_kinds_for(Profession) -> &'static [JobKind]` — Farmer → `[Farm, Stockpile]`, Hunter → `[Stockpile]`, Bureaucrat → `[Build]`, Trader → `[Haul]`; `primary_skill_for`; `aggregate_wage_per_day(faction, prof)` summing `wage_signal[(kind, _)].ema_per_day` over the profession's kinds; `expected_wage(faction, prof, skills, capital_factor)` composing the three) plus shared `demote_profession_state(entity, ai, aq, reservations, commands)` extracted from the duplicated demote arms. **`faction_hunter_assignment_system`, `chief_bureaucrat_appointment_system`, and `faction_profession_system` (Farmer) now rank candidates by `(expected_wage, skill)`** — wage_signal dominates when populated, raw skill is the tiebreaker when the signal is empty. The Farmer system previously promoted in scan order; now it picks the highest-EV (then highest-Farming) `Profession::None` agent. The full unified `profession_choice_system` (single argmax across every profession with hysteresis + explicit survival override) remains a mechanical follow-up.

- **Phase 4b capital threading — shipped.** All three EV-ranking systems now compute per-agent `capital_factor(agent, carrier, tile, fid, household, prof, &ownership, &plots, &plot_index)` — averaging `tool_capital_factor` + `workshop_capital_factor` + `land_capital_factor` — and feed it into `expected_wage` in place of the prior 1.0 constant. Concretely: a weapon-bearing candidate lifts Hunter EV by ×1.17 (tool 1.5 averaged with workshop/land 1.0 → 1.167); a Market within `WORKSHOP_AFFINITY_RADIUS = 12` of an agent lifts Bureaucrat EV by ×1.17; a Farmer whose household holds an Agricultural plot under Leased/Sharecropping/Freehold tenure lifts EV by ×1.17. Household-owned workshops dominate village-owned ones (+1.0 vs. +0.5 on the workshop term). Queries grew by `&EconomicAgent, &Carrier, &Transform, Option<&HouseholdMember>` (all read-only) plus `Res<WorkshopOwnership>, Res<PlotIndex>, Query<&Plot>` SystemParams; mutation paths are unchanged. 530 tests pass (525 prior + 2 new linear-scaling + zero-signal tests + 3 pre-existing pure-helper tests confirming the composition).

- **Phase 5a (Crafter introduction + dispatch) — shipped.** `Profession::Crafter` joins the variant set in `person.rs`. The two `profession_choice` exhaustive matches were the only forced sites — every other code path uses `_ => …` catch-alls so the variant lands additively. Scaffolding activates: `WorkshopKind::affine_to(Crafter)` returns true for `Workbench | Loom`; `tool_profession(rid)` maps the catalog `tools` ID to Crafter; `job_kinds_for(Crafter)` returns `[Craft, Stockpile]`; `primary_skill_for(Crafter)` returns `Crafting`. The Phase 2 `tool_capital_factor` hand-slot probe is generalized — Hunter probes for `weapon`, Crafter for `tools`. New `chief_craft_assignment_system` (Economy, after `chief_bureaucrat_appointment_system`, every `CRAFTER_ASSIGNMENT_CADENCE = TICKS_PER_DAY/4`) targets `max(1, members × CRAFTER_MIN_RATIO (0.25))` Crafters capped at `members / CRAFTER_MAX_DIVISOR (3)` when the faction's `wage_signal[(Craft, _)].ema_per_day > 0`; on signal collapse target → 0 and crafters demote via the shared `demote_profession_state`. Capital factor is threaded (Workbench/Loom + `tools` resource). `chief_craft_assignment_system` is registered in `mod.rs` and ordered between bureaucrat and salary tick. Crafter affinity into `U_bid` scoring: `CRAFTER_AFFINITY_BONUS = 3.0` lifts a Crafter's paid-Craft posting bid above an equidistant generalist; the unpaid path's `profession_bias` gets a matching `(Crafter, Craft) = 0.5` arm (plus `(Crafter, Stockpile) = 0.1` for upstream material work). Phases 5b apprenticeship + Healer/Apprentice variants remain deferred. 531 tests pass (530 prior + 1 new workshop-affinity-Crafter; `job_kinds_for` and `primary_skill_for` tests expanded inline).

- **Crafter wage-signal hysteresis — shipped.** `chief_craft_assignment_system`'s `ema > 0.0` trigger was sensitive to single-payout noise (one ~5-currency contract folded into the EMA at `α ≈ 0.129` produces `ema ≈ 0.65` first day, decaying back below 0.1 after a few quiet days). Replaced with a deadband: `CRAFTER_WAGE_PROMOTE_FLOOR = 1.0` (sustained signal required for promotion); `CRAFTER_WAGE_DEMOTE_CEILING = 0.3` (signal must genuinely decay to demote); between thresholds, `target = current crafter count` (no churn). Implemented via pure helper `crafter_target_with_hysteresis(craft_ema, current_crafters, member_count) -> usize` so the deadband logic is unit-testable without a `World`. The system grew a pre-pass over members to count `current_crafters` per faction before the target loop. 542 tests pass (537 prior + 5 hysteresis-helper tests).

- **Phase 7 (root CLAUDE.md docs) — shipped.** New "Wage-aware labor market" section in root `CLAUDE.md`, sitting between "Land ownership" and "Simulation scheduling". Covers the end-to-end pipeline at the level a cross-cutting reader needs: chief postings paid from `faction.treasury` (Mixed/Market), `chief_wage_for(progress)` formula, escrow → claimant payout via `JobEscrowIndex`, `Earnings` ring, daily `WageEMA` fold (`α ≈ 0.129`, 5-day half-life), `PerceivedFactionWages` cross-faction gossip, `capital_factor` triad (tool / workshop / land), EV-ranked profession assignment across all four chief-driven roles (Farmer / Hunter / Bureaucrat / Crafter), Subsistence regression carve-out (empty `economic_policy` skips funding). Subsystem-local detail stays in `src/simulation/CLAUDE.md`; root section is the orientation map.

- **Phase 5b (Crafter apprenticeship — minimal core) — shipped.** New `src/simulation/apprenticeship.rs` module: `Profession::Apprentice` unit variant added (additive — every other code path uses `_ => …` catch-alls except the two `profession_choice` matches, which now return `[Craft, Stockpile]` / `SkillKind::Crafting` for Apprentices); paired components `ApprenticeOf { mentor }` + `MentorOf { apprentice }` enforce one-to-one mentor binding; `ApprenticeProgress { ticks, target_ticks = TICKS_PER_DAY × APPRENTICESHIP_DURATION_DAYS (30) }` is the daily-incremented ledger. Routing: `chief_craft_assignment_system` grew a pre-pass that collects per-faction `available_mentors` (Profession::Crafter + Skills[Crafting] ≥ `MASTER_THRESHOLD (100)` + no existing `MentorOf`); the apply loop, before stamping `Profession::Crafter` on a promote, checks the candidate's `Skills[Crafting]` — sub-`APPRENTICE_THRESHOLD (30)` candidates pop a master from the pool and become `Apprentice` instead. No-mentor fallback drops to direct Crafter so a fresh faction can bootstrap. `apprentice_progress_system` (Economy, daily, ordered after `chief_craft_assignment_system` in its own `add_systems` block — the prior Economy tuple is at Bevy's 20-elt ceiling) advances `progress.ticks` by `TICKS_PER_DAY`, graduates on `ticks >= target_ticks` (lifts `Skills[Crafting]` to `APPRENTICE_THRESHOLD` floor; despawns `ApprenticeOf` / `ApprenticeProgress` / matching `MentorOf`). Orphan path: a stale mentor link (master despawned or `MentorOf` torn off externally) demotes the apprentice back to `Profession::None`, discarding progress; the next chief pass may rebind. Apprentices count toward `current_crafters` (hysteresis target) but skip the Farmer / Hunter / Bureaucrat candidate pools via `_ => {}`. Healer apprenticeship + apprentice-fraction wage split (40% apprentice / 10% mentor fee / 50% refund) and the deliberate-practice 2× XP multiplier remain deferred. 547 tests pass (544 prior + 3 new: routing, graduation, orphan).
- **Phase 5b apprentice payout split — shipped.** `job_payout_system`'s per-claimant loop reads `ApprenticeOf` and routes the share through a 0.4 (apprentice) / 0.1 (mentor fee) / 0.5 (residual → refund) split. Mentor credit is skipped when `mentor == beneficiary` (mentor-funded posting edge case) or when the mentor entity no longer carries `EconomicAgent` (post-despawn). Both apprentice and mentor get matching `Earnings` ring entries + `ActivityEntryKind::WagePaid` activity-log lines at the reduced amounts. Residual stays in escrow via `paid_total += worker_pay + mentor_pay`; the despawn hook refunds the un-paid 0.5 to the beneficiary. New constants `WAGE_FRACTION_APPRENTICE = 0.4` and `WAGE_FRACTION_MENTOR_FEE = 0.1` live in `apprenticeship.rs` so the split is one-stop tunable. 548 tests pass (547 prior + 1 new — apprentice gets 4.0, mentor 1.0, poster ends at 95.0, currency invariant within `eps = 1e-3`).
- **Phase 5b deliberate-practice XP multiplier — shipped.** `apprenticeship::xp_with_apprentice_bonus(base, Option<&ApprenticeOf>)` multiplies a raw XP grant by `APPRENTICE_XP_MULT = 2` when the agent carries an `ApprenticeOf` link, returning `base` otherwise. Threaded into the three canonical Crafting-XP grant sites: `crafting.rs:808` (per-tick craft work `+1`) and `crafting.rs:831` (recipe completion `+recipe.crafting_xp`) — both within `apply_recipe_progress_system`'s mutable agent query (`Option<&ApprenticeOf>` added) — plus `corpse.rs:267` (butchering `+5`) within `butcher_task_system`'s agent query. Builder XP / Mining XP sites are unchanged (apprenticeship targets crafts only). 551 tests pass (548 prior + 1 integration test + 2 pure-helper tests — the multiplier doubles a 5-XP grant from default Skills(5) into 5→15 for the apprentice vs. 5→10 for a peer).
- **Phase 4b survival override — shipped.** New `FARMER_SURVIVAL_FLOOR = 16.0` constant in `faction.rs`. The three non-Farmer assignment systems each gain a per-head food check at target-build time: when `faction.storage.food_total() / member_count < FARMER_SURVIVAL_FLOOR`, the target is forced to 0. Specifically: `faction_hunter_assignment_system` factors `survival` into the `has_tech && adults > 0 && !survival` precondition; `chief_bureaucrat_appointment_system` adds the same survival gate (in addition to the existing `bureaucrat_treasury_empty_streak` quit path); `chief_craft_assignment_system` early-returns `target = 0` before the wage-EMA hysteresis check fires. Existing incumbents demote via the shared `demote_profession_state` teardown. Apprentices stay bound — the override targets *new promotion*, not active links. Two existing tests (`bureaucrat_promoted_then_demotes_when_treasury_drains`, `phase5b_low_skill_crafter_promotion_routes_to_apprentice`) needed `seed_faction_food` calls to clear the new floor; that's the documented test-fixture pattern. 552 tests pass (551 prior + 1 new — hunter starving faction demotes to None on cadence fire).
- **Phase 4b asymmetric demotion buffer — shipped.** `HUNTER_DEMOTE_BUFFER = 1` / `BUREAUCRAT_DEMOTE_BUFFER = 1` on the demote arm of `faction_hunter_assignment_system` / `chief_bureaucrat_appointment_system`. Demotion only fires when `current > want.saturating_add(BUFFER)`; promotion is unchanged (eager on any shortfall). The `want == 0` arm explicitly bypasses the buffer so survival override / treasury-quit / tech-loss can still force full stand-down. Crafter's existing EMA-band hysteresis (`crafter_target_with_hysteresis`) is the analogous pattern; this extends pro-stability bias to the two systems whose target metric (prey density × martial / `member_count × ratio`) was prone to ±1 rounding jitter across cadence cycles. 553 tests pass (552 prior + 1 new — 2 hunters held in place at target=1 with the buffer absorbing the unit excess).
- **Phase 6 U_bid bias fixes (regression on chief postings) — shipped.** When `chief_post_funding_system` funds chief postings at `CHIEF_MARGIN = 0.5` of market value, household / individual contracts at full value consistently outscore them on `expected_reward = p.reward × wealth_mod`. Workers stopped claiming chief postings entirely in Mixed/Market mode. Fix: thread two new bias terms into `job_claim_system`'s paid `U_bid` arm — a `priority_bonus = p.priority × 0.01` (chief postings post at `priority = 200` vs. household `180` vs. individual `100`, so chief gets a +2.0 cushion that covers the half-margin gap at equal reward); and a `Disposition.entrepreneurial` multiplier `expected_reward = p.reward × wealth_mod × (1.0 + ent/255)` so per-agent variance pulls high-entrepreneurial workers toward paid contracts and low-entrepreneurial ones toward communal labor. Default median Disposition → 1.5× — matches the `EarnIncomeScorer`'s formula end-to-end. `Disposition` is now a `Query<Option<&Disposition>>` parameter on `job_claim_system` (`unwrap_or(1.5)` default = same as median agent, so the system is robust against pre-Phase-6 spawn sites that miss the component). New tests: `chief_postings_still_claimed_subsistence_after_earnincome`, `chief_postings_still_claimed_market_after_earnincome`, `chief_priority_bonus_keeps_chief_postings_competitive` (the last pins chief priority 200 outscoring household priority 100 at equal reward 5.0 / equal distance 0). 563 tests pass.
- **Phase 6 GoalScorer / Disposition infrastructure — shipped (in-plan).** The peer plan that was supposed to deliver `GoalScorer` + `Disposition` didn't ship; the plan called for folding their absence into a degraded procedural form (which landed first). With the user pushing for the proper form to land here, new module `src/simulation/goal_scorers.rs` houses the full plumbing: `Disposition { entrepreneurial, gregariousness, curiosity, martial }` Component (all `u8` on `[0, 255]`, default 128, scattered by `fastrand::u8(..)` at every spawn site in `person.rs` and stamped at default in `test_fixture.rs`); `GoalClass` enum (`Survival > Subsistence > Safety > Belonging > Esteem > Enterprise > Discretionary`) — `Ord` derived so registry argmax can sort `(class, score)` tuples cleanly; `GoalScore { goal, class, score, reason }` return type; `GoalScorer` trait (`score(&self, ctx: &GoalScoringContext) -> Option<GoalScore>` + `name()`); `GoalScoringContext` SystemParam-free read bundle holding `Entity / agent_tile / now / Needs / Profession / Skills / Disposition / EconomicAgent / FactionMember / FactionData / JobBoard`; `GoalScorerRegistry` Resource (`Vec<Box<dyn GoalScorer>>` + `best(ctx)` argmax helper). `register_default_scorers` pushes `EarnIncomeScorer` today; future scorers (Socialize / Esteem / HealSeeker / etc.) are one `registry.scorers.push(Box::new(...))` away. `EarnIncomeScorer` proper implementation: `score = posting.reward × skill_competence(primary_skill) × disposition.earn_income_multiplier()` where the multiplier `= 1.0 + entrepreneurial / 255` (median 1.5×, max 2.0×, min 1.0× — exactly matching the plan's `(1 + disposition.entrepreneurial / 255)` formula); declines (returns `None`) for `None / Apprentice` professions, Subsistence factions (empty `economic_policy` AND empty `wage_signal`), or no matching unclaimed paid posting; emits at tier `Enterprise`. `earnincome_goal_override_system` refactored to consume `GoalScorerRegistry::best(&ctx)` instead of hardcoding the EarnIncome logic — picks any `Enterprise+` tier scorer's pick and applies it; lower-tier scorers (Discretionary placeholders) are filtered out so the legacy cascade still drives Subsistence / Safety / Survival. Inspector surfaces `Disposition` line in the Wage & Labor section (`ent N • greg N • cur N • mar N`) and prints the `EarnIncome ×N.NN` multiplier on the EV-table header so the operator can see at a glance how disposition-biased the agent is. 5 new tests pass (3 helper tests in `goal_scorers::tests` + `registry_argmax_breaks_ties_by_class_first` integration + `phase6_earnincome_scorer_respects_disposition` end-to-end pinning the 2× multiplier band). 560 tests pass (555 prior + 5).
- **Phase 6 EarnIncome procedural fold-in — shipped.** Peer plan's `GoalScorer` registry + `Disposition` component remain undelivered, so the plan's documented degraded form lands: a new `earnincome_goal_override_system` (ParallelA, after `goal_update_system` and `mobile_state_goal_gate_system`) walks every claim-less, undrafted, professioned agent in a Mixed/Market faction and rewrites their fallback gather goal (`GatherFood / GatherWood / GatherStone`) to the highest-reward unclaimed paid posting's `JobKind::to_goal()`. Ranking is `posting.reward × skill_competence(primary_skill_for(prof))` — the same scoring the plan's full `EarnIncomeScorer` calls for, modulo the entrepreneurial `Disposition` multiplier (deferred along with the peer plan's component). Travel cost is delegated to `job_claim_system`'s `U_bid`, which already penalises distance at claim time. Profession::None / Apprentice opt out (Nones drift on subsistence reflexes; Apprentices route earnings via the wage-split path already wired by their mentor's posting). Subsistence factions opt out via the `economic_policy.is_empty() && wage_signal.is_empty()` discriminator — chief still allocates communal labor exactly as before. Tier matches `GoalClass::Enterprise`: above generic gather (which is the only goal the override rewrites), below Survive / Sleep / Socialize / Play / TameHorse / personal-Build / Craft (which fired upstream and aren't gather goals). New test `phase6_earnincome_override_rewrites_gather_to_craft` pins a Crafter in a Market-preset faction with a 25-currency paid Craft posting switching `GatherFood → Craft` with reason "Earning Income". 555 tests pass (554 prior + 1).
- **Phase 5b-stretch Healer scaffolding — shipped.** `Profession::Healer` variant added additively (matches the Phase 5a Crafter pattern — scaffolding now, behavior when the heal-job pipeline lands). Recognized by `profession_choice`: `job_kinds_for(Healer) = [JobKind::Craft]` (the closest analogue until a dedicated `JobKind::Heal` lands), `primary_skill_for(Healer) = Medicine`, `faction_cap_for(Healer) = adults / CRAFTER_MAX_DIVISOR` mirroring Crafter (skilled-service ceiling). `WorkshopKind::Shrine.affine_to(Healer)` returns true so the inspector EV table and the cross-switcher's `EV(Healer)` read a non-trivial capital factor when a household holds a Shrine. Apprenticeship plumbing extended: `ApprenticeProgress { target_profession }` field replaces the previous always-Crafter graduation — graduation reads `primary_skill_for(progress.target_profession)` to apply the floor and writes `*prof = progress.target_profession`, so a Healer-target apprenticeship would graduate to `Healer` writing `SkillKind::Medicine ≥ APPRENTICE_THRESHOLD`. No auto-promotion path exists today (no `chief_heal_assignment_system`, no Heal-job posting source), but the variant + plumbing land cleanly so the heal-job pipeline becomes a pure additive follow-up. Inspector EV table grows from 5 to 6 candidates. 554 tests pass (no regressions; scaffolding-only).
- **Phase 4b unified cross-profession switcher — shipped.** New `cross_profession_switch_system` in `profession_choice.rs` (Economy, daily, after every per-profession assignment system + `apprentice_progress_system`). For each employed agent in `{Hunter, Bureaucrat, Crafter}`, walks the other candidates in the set, computes `EV(target) = expected_wage(faction, target, skills, capital_factor(...))` and the agent's skill-regret cost, and switches if `EV(target) - regret > EV(current) × EV_SWITCH_HYSTERESIS (1.20)`. Switching cost helper `switching_cost_skill_regret(faction, current, peaks)` returns `peak[primary_skill] / SKILL_MAX × SKILL_REGRET_FRACTION (0.20) × aggregate_wage_per_day(current)` — a Hunter who peaked at Combat 200 pays a real EV penalty to leave Combat behind; a freshly-promoted Hunter at peak 5 barely feels it. Faction caps enforced via new `faction_cap_for(faction, target) -> Option<usize>` helper: `Hunter ≤ adults/2` (gated on chief-aware `HUNTING_SPEAR`), `Bureaucrat ≤ max(1, adults×0.05)` (gated on `state_funds_public_works`), `Crafter ≤ adults / CRAFTER_MAX_DIVISOR (3)`. Survival override (`per_head food < FARMER_SURVIVAL_FLOOR`) locks every non-Farmer target out — labor stands down for the Farmer ramp exactly as the existing systems do. Sub-`APPRENTICE_THRESHOLD` Crafter targets route through `Profession::Apprentice` with the same mentor-pool pattern as `chief_craft_assignment_system`. Plumbing: `Profession` now derives `Hash` so `AHashMap<(u32, Profession), usize>` headcount tables work; `LastSeenProfession` shadow + suppression in `profession_change_log_system` updated to silence any `* → Apprentice` transition (the dedicated `ApprenticeshipStarted` event handles the surfacing). New test `phase4b_cross_switch_hunter_to_crafter_on_wage_spread` pins the behavior: a low-Combat Hunter with moderate Crafting (60) in a 9-member faction with Craft EMA 20.0 / Stockpile EMA 1.0 switches directly to Crafter on the next daily pass. 554 tests pass (553 prior + 1).
- **Phase 6 (activity log for labor transitions) — shipped.** Three new `ActivityEntryKind` variants in `ui/activity_log.rs`: `ProfessionChanged { from, to }` (generic role transition), `ApprenticeshipStarted { mentor }` (mentor entity inlined for UI), and `ApprenticeshipGraduated` (Apprentice → Crafter). `ProfessionChanged` is emitted centrally by a new `profession_change_log_system` (Economy, after every profession-mutation system + `apprentice_progress_system`) which watches `Changed<Profession>` against a shadow `LastSeenProfession` component, so the promote/demote sites don't need to write the event individually — and a same-tick promote-then-graduate transition (e.g. apprentice graduates → also gets re-evaluated by the next pass) collapses to one final log entry. Apprenticeship-specific transitions (`None ↔ Apprentice`, `Apprentice → Crafter`) are silenced in the centralised emitter — the dedicated `ApprenticeshipStarted` / `ApprenticeshipGraduated` events carry the mentor info and verb the player wants instead. `chief_craft_assignment_system` emits `ApprenticeshipStarted { mentor }` immediately after binding the `ApprenticeOf` / `MentorOf` pair; `apprentice_progress_system` emits `ApprenticeshipGraduated` immediately after the profession write. Both events are player-faction-filtered by `activity_log_ingest_system` like every other entry. 553 tests pass (no test regressions; events are passive surfacing).
- **Phase 6 (inspector UI) — shipped.** New `WageInspectorParams` SystemParam in `ui/inspector.rs` (peaks / earnings / perceived-wages / apprenticeship / household queries + `WorkshopOwnership` and `PlotIndex` resources + a `Plot` query) keeps the main panel tuple under Bevy's per-tuple ceiling. **Profession line** surfaces apprenticeship state inline — Apprentices show `apprentice N/30 d (XX%)` and the mentor's entity index; live mentors show `mentoring #N`. **Skills section** now prints `cur / peak P (floor F)` per slot via the new `skills::skill_floor(peak) -> u32` public helper (factored out of `skill_decay_system` so the inspector and the decay system can't drift); mastered skills (peak ≥ `SKILL_MASTERY_LINE`) render in green. **New "Wage & Labor" collapsing section** between Stats and Knowledge: 24-hour earnings rollup + total ring + last entry (`{kind} {rid} +{amount} @{tick}`); per-profession EV table (`Farmer / Hunter / Crafter / Bureaucrat / Trader` — `EV {ev:.2} (wage {agg} × comp {c} × cap {cap})` with current profession highlighted yellow and zero-EV rows greyed); own-faction `wage_signal` top-6 rows sorted by EMA per game-day with sample count; cross-faction perceived wages from gossip (top-6 by EMA with observation-age in days). EV computation reuses `capital::capital_factor` end-to-end so the inspector readout matches what `chief_craft_assignment_system` / `faction_hunter_assignment_system` / `chief_bureaucrat_appointment_system` / `faction_profession_system` see. Phase 6's `EarnIncomeScorer` (depends on the deferred `GoalScorer`/`Disposition` traits) is not shipped — the inspector lands independently of the scorer registry. 553 tests pass (no test regressions; UI is read-only).
- **Phases 4b (full unification + per-agent EV hysteresis), 5b-stretch (Healer), 6 EarnIncomeScorer — deferred.** Single argmax across all professions with explicit per-agent EV hysteresis (`EV(p*) > EV(current) × 1.20`); Healer/Apprenticeship-target dimension; HTN scorer hooks — pending follow-up sessions.

## Context

The existing plan diagnoses the right problem (profession assignment ignores wages/skills/capital/switching costs) and the right precondition bug (`JobCompletedEvent` has zero consumers, so workers are never paid). But several claims are stale or wrong, and one whole sub-system (`Profession::Crafter` + craft-profession dispatch) is assumed to exist when it doesn't. This v2 fixes those, adds an explicit chief-wage-source phase (chosen: faction treasury, Mixed/Market only), and broadens apprenticeship to crafts + medicine per user direction.

## Audit findings — what the original got wrong

| Claim in v1 plan | Reality |
|---|---|
| `Profession::Crafter` exists | **No.** Only `None / Farmer / Hunter / Bureaucrat / Trader` (`person.rs:52-70`). Craft work is goal-driven via `AgentGoal::Craft` + CraftOrder, no profession gate. Phase 5 must *introduce* Crafter before apprenticeship makes sense. |
| `has_tool()` at `gather.rs:572` | **No such function.** Equipment checks go through `Equipment::has_resource(rid)` against `core_ids::weapon() / shield() / armor()`. No profession-affine tool concept yet. |
| Granular tools (Hoe, Bow, Hammer, Awl, Loom-shuttle) | **No.** `core.ron` has generic `"tools"` and `"weapon"` resources. Affinity must come from catalog tags or new resource entries, not enum variants on `Item`. |
| `WorkbenchMap.by_faction[household_id]` | **No `by_faction` index.** `WorkbenchMap(AHashMap<(i32,i32), Entity>)` is tile-keyed (`construction.rs:78`). Workshops carry no `OwnedBy` component either; they aggregate to whatever faction posted the blueprint, but nothing reads that today. |
| `BuildIntent::Workbench / Forge / Loom` | **No.** Workshops are posted as `BuildIntent::Single(BuildSiteKind::Workbench)` (`construction.rs:1931-1949`). No dedicated intent for household workshop posting. |
| Phase 6 `EarnIncomeScorer` lands cleanly | **Hard dependency.** `GoalScorer` trait and `GoalRegistry` resource don't exist; they are deferred Phase 4 of `autonomous-food-labor-market-completion.md`. `Disposition` same story (its Phase 5). v2 lists Phase 6 as conditional on those shipping first. |

What the original got right (verified): the `JobCompletedEvent` ghost, `JobEscrow.on_remove` refund semantics (`jobs.rs:410-431`), `pay()` signature (`transactions.rs:165`), profession-system anchor points (`faction.rs:172 / 329 / 972`), `Skills([u32; 8])` shape, sharecrop yield routing live in `gather.rs` + `land.rs`, and the catalog `trade_base_value` source-of-truth pattern.

## Design choices baked in (user-confirmed)

- **Chief wage source:** Mixed/Market chief postings pull from `FactionData.treasury`; Subsistence chief stays unpaid (`reward = 0`). Couples coordination to fiscal health — bankrupt factions can't direct paid labor and fall back to communal/autonomous work.
- **Apprenticeship scope:** Crafts + Medicine (deliberate-practice professions). Farmer/Hunter/Builder/Trader/Bureaucrat learn by doing.
- **Skill decay:** slow drift to a peak-derived floor; daily linear at one unit/day is too aggressive — switched to a half-life model.
- **Capital ownership:** add a real `OwnedBy(u32)` component on workshop entities + a `WorkshopOwnership` resource indexed by faction. Household-keyed workshops then work without a special case.

## Phasing

Ship order: **0 → 1 → 2 → 3 → 4a → 4b → 5a → 5b → 6 → 7**. Each is independently testable and lands measurable value.

### Phase 0 — Wage payout (precondition; everything else is dead weight without this)

The bug, precise form:

- `JobCompletedEvent { job_id, faction_id, kind }` is emitted at 6 sites (`jobs.rs:1266, 2624, 2759, 2832, 2933` + `construction.rs:3652`). No `EventReader<JobCompletedEvent>`.
- `JobEscrow` lives on a sidecar entity. `JobEscrow.on_remove` (`jobs.rs:410-431`) refunds `beneficiary` (the poster) when `amount > 0` at despawn. Successful payout is the responsibility of the system that *fires* completion: zero `amount` and despawn the sidecar.
- The completion event doesn't carry a worker entity. `JobPosting.claimants: Vec<Entity>` does (`jobs.rs:249-275`); the worker is the claimant who actually completed (one or more depending on multi-worker postings).

**Implementation: `job_payout_system`** (`jobs.rs`, Sequential set, exclusive `&mut World` — `pay()` is `&mut World`):

1. Drain `Events<JobCompletedEvent>` into a local `Vec` (avoid event-reader vs world-write conflict).
2. For each event, look up `JobPosting` via `JobIndex` or `world.entity(job_id)`. Read `claimants` + the sidecar `JobEscrow.amount, beneficiary`.
3. **Split rule** (drives multi-worker postings — Build, Stockpile, Farm):
   - Total payout = `escrow.amount`.
   - For each completing claimant, pay `amount / completed_claimants` via `pay(world, beneficiary, worker, share)`.
   - Apprentice claimants (Phase 5) get `WAGE_FRACTION_APPRENTICE = 0.4` of their share, with the remaining 0.6 going to mentor if `MentorOf` link exists, else returns to `beneficiary`.
4. Decrement `escrow.amount` by total paid. Despawn sidecar — the `on_remove` hook sees `amount == 0` and no-ops.
5. Push an `Earnings` entry on each paid worker (Phase 3 component, introduced here as a stub `VecDeque<EarningEntry>` cap 16: `{ job_kind, target_rid: Option<ResourceId>, amount, tick }`).
6. Emit `ActivityEntryKind::WagePaid { worker, kind, amount }` for UI surfacing.

**Currency invariant** stays intact: poster→escrow→worker is a closed transfer; the existing `CurrencySnapshot::capture` assertion in `test_fixture.rs` already covers `JobEscrow.amount` so the invariant holds end-to-end through payout.

**Where completion fires today:** each of the 6 sites already runs in Sequential or Economy ordering. `job_payout_system` runs after **all** of them by ordering it `.after()` each producer system (or just `last_in_set(Sequential)` since the `Events<>` ring delivers across schedules).

**Test gate:** new fixture helper `TestSim::post_paid_contract(faction, poster, recipe, qty, reward)` + `assert_currency_delta(worker, +reward)` after running through completion. Currency-snapshot invariant assertion across the whole flow.

### Phase 1 — Skill model: peaks, ceiling, half-life decay

`skills.rs` (51 lines today) gets a careful upgrade:

```rust
pub const SKILL_MAX: u32 = 255;
pub const SKILL_FLOOR_BASE: u32 = 5;
pub const SKILL_MASTERY_LINE: u32 = 80;
pub const SKILL_MASTERED_FLOOR: u32 = 30;
pub const SKILL_PEAK_FLOOR_FRACTION: f32 = 0.30;
pub const SKILL_DECAY_HALF_LIFE_DAYS: u32 = 90;   // *unused* time to halve toward floor

#[derive(Component, Clone, Copy, Default)]
pub struct SkillPeaks(pub [u32; SKILL_COUNT]);

#[derive(Component, Clone, Copy)]
pub struct SkillUseTicks(pub [u32; SKILL_COUNT]); // last tick each skill earned XP
```

- `Skills::gain_xp(...)` rewritten to `Skills::practice(&mut self, peaks, use_ticks, kind, amount, tick)` so every XP grant atomically (a) clamps to `SKILL_MAX`, (b) bumps peak, (c) stamps `use_ticks[kind] = tick`. Migrate every call site via grep — there are ~12 of them across gather/dig/construction/crafting/teaching/combat.
- `skill_decay_system` (Economy set, every `TICKS_PER_DAY`):
  - For each agent, each skill: if `now - use_ticks[kind] >= TICKS_PER_DAY`,
  - `floor = peak >= SKILL_MASTERY_LINE ? SKILL_MASTERED_FLOOR : max(SKILL_FLOOR_BASE, (peak as f32 * 0.30) as u32)`,
  - `skill = floor + ((skill - floor) as f32 * 0.5_f32.powf(1.0 / SKILL_DECAY_HALF_LIFE_DAYS as f32)) as u32` (slow exponential decay).
  - Skill stays at or above `floor` always.
- Spawn sites: `PersonBundle` (in `person.rs`) gets `SkillPeaks::default()` + `SkillUseTicks([0; 8])`. Newborn inheritance unchanged (only `Skills` matters at birth; peaks reset to current).

### Phase 2 — Capital recognition (tools, workshops, land)

#### 2a. Profession-affine tools via catalog tags

The clean version: catalog-driven, not enum-driven.

- `assets/data/resources/core.ron`: split today's generic `"tools"` into kind-specific entries: `hoe`, `awl`, `loom_shuttle`, `hammer`, `bow`. Each carries `tags: ["tool", "<profession-tag>"]`. `weapon` is already separate.
- `core_ids::hoe()`, `core_ids::awl()`, etc. (accessor pattern matches existing weapon/shield).
- `ResourceId::tool_profession(catalog) -> Option<Profession>` — reads tags. `"tool", "farming"` → Farmer; `"tool", "crafting"` → Crafter; `"tool", "building"` → Builder (skill-based; no profession yet, factor falls back); `"weapon", "hunting"` → Hunter.
- Existing craft recipes that produce `"tools"` get updated to produce specific tool kinds, gated on tech where appropriate (e.g. Hoe gated on `AGRICULTURE` or similar).
- `tool_capital_factor(agent_inventory, profession) -> f32`: 1.0 base; +0.5 if any inventory item maps to `profession` via `tool_profession`. **No durability** — explicitly deferred.

#### 2b. Workshop ownership and affinity

- New component `OwnedBy(u32)` (faction_id) on workshop tiles (Workbench, Forge, Loom, Granary, Shrine, Market, Barracks, Monument). Set at spawn from `BuildIntent.poster_faction_id` (today implicit via `chief_directive_system`; thread it through).
- New resource `WorkshopOwnership` indexing `AHashMap<u32, Vec<Entity>>` (faction → workshop entities). Populated by `on_add` hook for `OwnedBy`, drained by `on_remove`.
- `workshop_capital_factor(agent, profession) -> f32`:
  - 1.0 base.
  - +0.5 if agent's village faction owns a profession-affine workshop within `WORKSHOP_AFFINITY_RADIUS = 12` of the agent.
  - +1.0 if agent's household sub-faction (per `HouseholdMember`) owns one.
  - Profession→workshop map: Crafter→Workbench/Loom/Forge, Bureaucrat→Market, Healer (future)→Shrine.

#### 2c. Land affinity (already shipped)

`land_capital_factor(agent, Farmer)` = 1.0 + 0.5 if agent's `HouseholdMember.household_id` holds (`Tenure::Freehold | Sharecropping`) any `ZoneKind::Agricultural` plot, queried via `PlotIndex.by_faction_hash` + tenure walk. Sharecrop already routes yield correctly (`gather::lookup_sharecrop_split`, `land.rs:835-856`) so this is pure read-side.

### Phase 3 — Earnings memory + faction wage signal + gossip

#### Per-agent
- `Earnings(VecDeque<EarningEntry>)` ring component, cap 16. Pushed by Phase 0. Read by inspector + wage-signal aggregator.

#### Per-faction
- `FactionData.wage_signal: AHashMap<(JobKind, Option<ResourceId>), WageEMA>`
- `WageEMA { ema_per_tick: f32, last_update: u32 }`
- Keying by `(kind, rid)` separates `Stockpile{wheat}` from `Stockpile{wood}` (the v1 plan conflated these).
- `faction_wage_signal_system` (Economy, daily): for each faction, sums each `(kind,rid)` payout from that day's Earnings entries across members, divides by total ticks worked (approx: # claimants × duration → approximate via `JobClaim.posted_tick` to completion), folds into EMA with **α = 1 − 0.5^(1/5) ≈ 0.129** (5-day half-life). EMA decays toward zero when no payouts arrive.

#### Cross-faction (information friction)
- `PerceivedFactionWages` Component on agents: `AHashMap<(u32 /*fid*/, JobKind, Option<ResourceId>), (f32, u32 /*tick*/)>`.
- Gossip merge: piggyback `Socialize` task in `social_fill_system` / `awareness_gossip_system` (`memory.rs`/`gossip.rs`). When two agents socialize, each merges the other's most recent ≤4 wage entries with `tick` decay penalty.
- Migration and cross-faction posting choices (future) read `PerceivedFactionWages`; same-faction work reads `wage_signal` directly. **No global broadcast** — wage spread between settlements is real economic friction.

### Phase 4 — Chief-funded posting wages + EV profession choice

#### 4a. Chief postings pay from faction treasury (Mixed/Market only)

`chief_job_posting_system` (`jobs.rs`) is the only point of authority. Today `reward = 0` unconditionally. After:

- For each posting type the chief is about to create, consult `faction.economic_policy[target_rid].chief_allocates_labor`. (Already used elsewhere — see policy_gate.)
- If chief is allocating but `caps.income.household_skim_pct > 0` (Mixed/Market), set `reward = chief_wage_for(kind, target_rid, qty, &faction.wage_signal, &catalog)` and debit `faction.first_settlement_or_treasury(...)` by that amount, creating a `JobEscrow` with `beneficiary = chief_entity`. If treasury can't cover the wage, fall back to `reward = 0` (free communal labor — represents fiscal distress).
- `chief_wage_for(...)`:
  - For Stockpile{rid}: `catalog.trade_base_value(rid) * qty * CHIEF_MARGIN (0.5)` — chief pays half of market value to encourage uptake without bidding wars.
  - For Build: fixed `CHIEF_BUILD_WAGE_PER_DAY (3.0) * expected_days` where expected_days = sum of slot quantities / typical fill rate. Bounded by `CHIEF_BUILD_WAGE_CAP (30.0)`.
  - For Craft: `recipe.output_value(catalog) * CHIEF_MARGIN`.
  - For Farm: fixed `CHIEF_FARM_WAGE_PER_DAY * expected_days`.
- Subsistence factions (policy map empty) → unchanged, `reward = 0`. No bootstrapping fragility.

Currency invariant grows by `treasury.currency` flow into escrow — already covered by snapshot.

#### 4b. Unified `profession_choice_system`

Replaces the *cores* (not the cadence wrappers) of `faction_profession_system`, `faction_hunter_assignment_system`, and `chief_bureaucrat_appointment_system`. Each cadence wrapper stays as the trigger; the inner allocation logic delegates to a shared per-agent EV computation.

Per agent, per allocation pass:

```
skill_competence(s) = 0.2 + (s.min(SKILL_MAX) as f32 / SKILL_MAX as f32) * 0.8
                           // floor 0.2 so newbies aren't useless

capital_factor(agent, p) =
    (tool_capital_factor(agent, p) + workshop_capital_factor(agent, p) + land_capital_factor(agent, p)) / 3.0

expected_wage(agent, p, faction) =
    faction.wage_signal.get_aggregate(job_kinds_for(p))
        * skill_competence(skill_of(p))
        * capital_factor(agent, p)

expected_value(agent, p) =
    expected_wage(agent, p) * EXPECTED_TENURE_TICKS         // 60 game-days
    - switching_cost(agent.current, p)

switching_cost(current, target) =
      apprenticeship_opportunity_cost(target)               // 0 unless target is Crafter/Healer + skill < 30
    + capital_abandonment(agent, current, target)           // sum of tied capital factor * EV_TIED
    + skill_regret(agent.skill_peaks[skill_of(current)])    // pv of decayed earnings if peak forgotten
```

**Decision rule:**

1. Compute EV for every `p ∈ {None, Farmer, Hunter, Crafter, Trader, Bureaucrat, Healer}` (Healer = Phase 5b stretch).
2. **Survival override** (preserve `faction_profession_system` semantics): if `faction.storage.food_total() / member_count < FARMER_SURVIVAL_FLOOR (16.0)`, force at least one Farmer; if no current Farmer exists, promote the agent with max `skill_competence(Farming)` regardless of EV.
3. **Faction caps** (post-filter): Hunter ≤ `adults/2`; Bureaucrat ≤ `member_count * BUREAUCRAT_MAX_RATIO` and gated on `state_funds_public_works`; Crafter ≤ `adults/3`. Caps applied by sorting agents by EV(target) desc and accepting until cap reached.
4. **Hysteresis**: switch only if `EV(p*) > EV(current) * 1.20` AND `EV(p*) > 0`.
5. **Apprenticeship gate** (Phase 5): if target ∈ {Crafter, Healer} and `skill < APPRENTICE_THRESHOLD = 30`, route through Apprentice variant instead of direct switch.

**Cadence:** runs at `PROFESSION_REVIEW_CADENCE = TICKS_PER_DAY/4` (same as today's farmer/hunter cadence). Per-game-day budget per faction so it doesn't recompute for every agent every cadence cycle — round-robin a bucket of `member_count / 4` agents per tick.

**Old systems retired:** the cores of `faction_profession_system:972` and `faction_hunter_assignment_system:172` and `chief_bureaucrat_appointment_system:329` get replaced; the survival floor stays as a hard override outside the EV path. Demote-teardown plumbing (`aq.cancel()`, strip `Carrying`, reset `task_id`) is reused — extract into `demote_profession(world, entity, old_prof)` helper called by both the legacy fallback and the new system.

### Phase 5 — Introduce Crafter + Apprenticeship

#### 5a. `Profession::Crafter` + dispatch integration

`person.rs::Profession` gains `Crafter` (and stretch `Healer`):

```rust
pub enum Profession {
    None, Farmer, Hunter, Bureaucrat, Trader,
    Crafter,
    Healer,                                     // Phase 5b — Medicine apprenticeship
    Apprentice {
        target: ApprenticeTarget,               // Crafter | Healer
        mentor: Option<Entity>,
        progress_ticks: u32,
        target_ticks: u32,                      // TICKS_PER_DAY * 30
    },
}
```

**Crafter dispatch integration** — without this, the profession is decorative:

- `chief_job_posting_system` Craft branch: prefer Crafter claimants when scoring `U_bid` — add `+CRAFTER_AFFINITY_BONUS (3.0)` to bids where the claimant's profession matches.
- `htn_work_on_craft_order_dispatch_system`: existing `profession_gate` mechanism (per `htn.rs` Method trait) — add a `profession_gate: Some(Profession::Crafter)` on `WorkOnSatisfiedCraftOrderMethod` with a softer fallback (still allows non-Crafter claim, but Crafter ranks higher).
- `chief_craft_assignment_system` (new, mirrors hunter pattern): targets `max(1, adults/4)` Crafters when `wage_signal[(Craft, _)].ema > 0` (i.e., there's craft work being paid for). Demotes when signal collapses.

#### 5b. Apprenticeship — crafts + medicine

```rust
pub enum ApprenticeTarget { Crafter, Healer }
```

**Entry conditions** (Phase 4 decision):
- Target ∈ {Crafter, Healer}.
- `Skills[target_skill] < APPRENTICE_THRESHOLD = 30` where `target_skill = Crafting` for Crafter, `Medicine` for Healer.
- Mentor available: same-village agent with `Skills[target_skill] >= MASTER_THRESHOLD = 100` and `Profession ∈ {Crafter, Healer}` (accepting apprentices is implicit; per-mentor cap of 1 enforced).
- No mentor available → fall back to `Profession::None`; Phase 4 re-evaluates next cadence.

**Apprenticeship lifecycle** (`teaching.rs` extension, mirrors `TeachingPair` pattern):
- New components: `MentorOf(Entity)` on master, `ApprenticeOf(Entity)` on apprentice. One-to-one (per-mentor cap 1).
- `apprentice_progress_system` (Economy, daily):
  - If both idle: emit paired `Task::Read { tech: era_tech_for(target) }` on apprentice + `Task::Teach { tech }` on mentor.
  - Skill XP from any craft/medicine activity granted at **2.0× rate** to apprentice (deliberate-practice multiplier; multiply through `Skills::practice`).
  - `progress_ticks += TICKS_PER_DAY` per day.
  - On completion (`progress_ticks >= target_ticks = TICKS_PER_DAY*30`): set `Skills[target_skill] = max(current, APPRENTICE_THRESHOLD)`, set profession to `target`, despawn the `MentorOf`/`ApprenticeOf` links.
- **Apprentice wage scaling** (Phase 0 hook): in `job_payout_system`'s per-claimant split, if claimant has `ApprenticeOf(mentor_e)`, pay claimant `share * 0.4` and mentor `share * 0.1` (mentor fee), refund `share * 0.5` to escrow beneficiary (cheaper labor = poster keeps half).
- **Abort path:** survival override (`faction.storage.food_total < members * 4`) demotes ALL apprentices in the faction to Farmer for the duration. Progress is paused, not lost — `progress_ticks` survives. Mentor link dissolved only if abort > 1 game-week (auto-rebind on next eligibility pass).

### Phase 6 — HTN scorer + UI (depends on peer plan)

Conditional on `autonomous-food-labor-market-completion.md` Phase 4 (scorer registry) and Phase 5 (Disposition) shipping. If those don't land in time, fold Phase 6 into the existing `goal_update_system` cascade as a procedural branch — degraded but functional.

- **`EarnIncomeScorer`** (`goal_scorers.rs`): `score = best_posting.reward * skill_competence(p) * (1 + disposition.entrepreneurial / 255.0) - travel_cost`. Tier = `GoalClass::Enterprise`.
- **Inspector** (`ui/inspector.rs`): Skills + SkillPeaks bars (with floor line), per-profession `expected_wage` table, current Profession (with apprentice progress bar if applicable), last-30-days Earnings total, gossip-perceived wages for other factions.
- **Activity log**: emit on wage payout, profession change, apprenticeship start/end, mentor binding.

### Phase 7 — Documentation

- `src/simulation/CLAUDE.md` — replace profession-assignment paragraph with the EV model; document skill decay (half-life formula, peaks, mastery floor); document `OwnedBy` + `WorkshopOwnership` + new tool-resource scheme; document apprenticeship (`MentorOf`/`ApprenticeOf`, scope = Crafter+Healer).
- `CLAUDE.md` (root) — Mixed/Market section: chief postings pay from faction treasury; agents pick profession by EV (wage × skill × capital − switching cost); skills decay slowly; Crafter/Healer gated by apprenticeship below skill 30.

## Files to touch

| File | Phase | What |
|---|---|---|
| `src/simulation/jobs.rs` | 0, 4a, 5b | `job_payout_system`; `chief_job_posting_system` reward funding; apprentice payout split |
| `src/economy/transactions.rs` | 0 | Helper `pay_from_escrow(world, escrow_entity, worker, share)` |
| `src/simulation/skills.rs` | 1 | `SkillPeaks`, `SkillUseTicks`, `Skills::practice`, `SKILL_MAX`, `skill_decay_system` |
| `src/simulation/items.rs`, `assets/data/resources/core.ron`, `src/economy/resources.rs` | 2a | Tool resources, `ResourceId::tool_profession` |
| `src/simulation/construction.rs` | 2b | `OwnedBy` component on workshops, set at spawn |
| `src/simulation/faction.rs` (new file `workshop_ownership.rs`) | 2b | `WorkshopOwnership` resource + add/remove hooks |
| `src/simulation/land.rs` | 2c | `land_capital_factor` helper (read-only) |
| `src/simulation/memory.rs`, `gossip.rs` | 3 | `PerceivedFactionWages`, gossip merge in `awareness_gossip_system` |
| `src/simulation/faction.rs:172, 329, 972` (new `profession_choice.rs`) | 4b | Shared `profession_choice_system`, extracted `demote_profession()` helper |
| `src/simulation/person.rs` | 1, 5a, 5b | `SkillPeaks`/`SkillUseTicks` in `PersonBundle`; `Profession::Crafter / Healer / Apprentice` variants |
| `src/simulation/htn.rs` | 5a | Crafter affinity bonus in craft-order claim scoring |
| `src/simulation/teaching.rs` | 5b | `MentorOf`/`ApprenticeOf` components, `apprentice_progress_system` |
| `src/simulation/test_fixture.rs` | 0, 1, 4b | `post_paid_contract`, `assert_currency_delta`, `advance_days`, `seed_master_in_skill` |
| `src/simulation/goals.rs`, `goal_scorers.rs` | 6 | `EarnIncomeScorer` (conditional) |
| `src/ui/inspector.rs` | 6 | UI panels |
| `src/simulation/CLAUDE.md`, `CLAUDE.md` | 7 | Doc updates |

**Reuse, don't reinvent:** `pay()` (`transactions.rs:165`), `JobEscrow` lifecycle hook (`jobs.rs:1105`), `Task::Read`/`Task::Teach` (`teaching.rs`), `PlotIndex.by_faction_hash` (`land.rs`), existing `awareness_gossip_system` merge (`memory.rs`), existing `policy_gate` mechanism on Methods (`htn.rs`), `chief_directive_system` per-faction posting cap.

## Verification

1. **Build/tests:** `cargo check && cargo test --bin civgame`. Baseline 503 tests stays green.

2. **Phase 0 acceptance:** TestSim — spawn one agent, post a paid Craft contract with `reward = 10.0`, run through completion. Assert `worker.currency == 10.0`, escrow despawned, currency snapshot invariant holds within `eps = 0.01`.

3. **Phase 1 acceptance:** TestSim — spawn agent, grant Crafting XP repeatedly until skill > 200; switch profession (Skills::practice stops). Fast-forward 90 game-days. Assert `skill ≈ 200 * 0.5 + floor * 0.5` (one half-life). Confirm floor = max(5, 200 * 0.3) = 60.

4. **Phase 2 acceptance:** `cargo run`, Mixed preset, Neolithic. Spawn agent with `hoe` in inventory; verify EV for Farmer is observably higher than for an identical agent without. Build a household-owned Workbench; verify EV for Crafter on that household's member is higher than for a non-household member.

5. **Phase 3 acceptance:** Run Market preset 5 game-days. Inspector shows `wage_signal[(Stockpile, wheat)].ema_per_tick > 0`. Two factions with different production mixes show different signals; an agent who socialized across factions has matching `PerceivedFactionWages`.

6. **Phase 4 acceptance (the real test):** `cargo run`, Mixed preset, era ≥ Neolithic. Spawn ≥ 12 founders; let run a game-month. Inspector should show:
   - Heterogeneous professions reflecting wages (no all-Farmer collapse).
   - Faction treasury draining as chief pays workers; rebuilding from `household_skim_pct`.
   - Agents with high `tool_capital_factor` or `land_capital_factor` stay in profession through wage dips that flip a capital-light agent.
   - Survival override fires when food/head drops below 16; at least one new Farmer promoted.
   - At least one agent in `Profession::Apprentice` with a paired mentor.

7. **Subsistence regression:** Same run, Subsistence preset. Profession churn matches main — survival floor dominates, no treasury drain, `wage_signal` stays empty, no apprentices.

8. **Currency invariant across all phases:** existing `assert_total_currency_invariant` covers `EconomicAgent.currency + FactionData.treasury + Settlement.treasury + JobEscrow.amount`. Run end-to-end across a 5-game-day Market simulation, assert invariant holds within `eps = 0.5`.

9. **Determinism:** seeded runs produce identical profession trajectories per agent across replays (Disposition ε-greedy is itself seeded; without Disposition the EV path is deterministic).

## Out of scope (explicit deferrals)

- **Tool durability/wear.** Static affinity in this plan; durability touches every gather/craft executor.
- **Cross-household workshop rental contracts.** Mechanically supported by existing job postings; no auto-posting system here.
- **Biological aging × skill.** Independent dimension (elder agents losing dexterity).
- **Inter-generational profession inheritance.** Children gain childhood XP from parents' professions — follow-up plan, hooks `pregnancy_system` + childhood maturation.
- **Faction-level capital investment.** Chief decides which workshop to build next via `chief_directive_system`; wage signal could feed it later but isn't wired here.
- **Phase 6 (UI + EarnIncomeScorer) only ships if peer plan's Phase 4-5 (GoalScorer / Disposition) lands.** Otherwise, fold Phase 6 changes into the existing `goal_update_system` cascade as a procedural branch and defer the UI polish.
- **Migration triggered by wage gradient.** Cross-faction `PerceivedFactionWages` is captured but no agent acts on it yet — natural follow-up once wages are stable across factions.
