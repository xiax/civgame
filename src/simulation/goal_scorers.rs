//! Phase 6 (wage-aware-labor-market-v2): `GoalScorer` trait, tier
//! taxonomy, and `Disposition` component — the plumbing the plan
//! originally deferred to a "peer plan" that didn't ship. With the
//! infrastructure native here, `EarnIncomeScorer` upgrades from the
//! degraded procedural form to a proper goal-scorer entry that the
//! goal-override dispatcher can argmax over alongside future scorers
//! (`SocializeScorer`, `EsteemScorer`, future `HealSeekerScorer` for
//! injured agents, etc.).
//!
//! Design intent matches the plan:
//! - Each scorer reads a per-agent `GoalScoringContext` (needs,
//!   profession, skills, disposition, faction view, job board) and
//!   returns an optional `GoalScore { goal, class, score, reason }`.
//! - Tier (`GoalClass`) breaks ties between qualitatively different
//!   needs. `Survival` (life-or-death) beats `Subsistence` (food/
//!   shelter floor) beats `Safety` (raid/defend) beats
//!   `Belonging`/`Esteem` (social/status) beats `Enterprise` (paid
//!   work / commerce) beats `Discretionary` (play / idle).
//! - Within a tier, raw `score` ranks. Different scorers in the same
//!   tier can compete (e.g. EarnIncome at `Enterprise` against a
//!   future TradeArbitrage scorer that pursues market gaps).
//!
//! The registry is consumed directly by `goals::goal_update_system` —
//! at most one scorer wins per agent per tick, and fallback imperative
//! logic only remains for synthetic / SOLO agents without faction data.

use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::simulation::faction::{FactionData, FactionMember};
use crate::simulation::goals::AgentGoal;
use crate::simulation::jobs::{JobBoard, JobKind};
use crate::simulation::needs::Needs;
use crate::simulation::person::Profession;
use crate::simulation::skills::Skills;
use crate::simulation::utility_curves::{
    disposition_lift, hunger_utility, play_utility, sleep_utility, social_utility, thirst_utility,
};

/// Per-agent psychological profile — the Big-Five-ish personality the
/// plan's `EarnIncomeScorer` calls for via the `entrepreneurial`
/// multiplier. Stored as `u8`s on the `[0, 255]` band to match every
/// other trait in the simulation. `Default::default()` leaves every
/// trait at `128` (median); per-agent rolls at spawn time scatter
/// these so populations have heterogeneous goal preferences without
/// every scorer needing to special-case "missing component" handling.
#[derive(Component, Clone, Copy, Debug)]
pub struct Disposition {
    /// Drives the `EarnIncomeScorer` multiplier — `score *=
    /// (1.0 + entrepreneurial / 255)`. Entrepreneurial agents
    /// preferentially chase paid contracts over generic subsistence
    /// gather when both are viable; cautious agents stick with the
    /// safe communal default.
    pub entrepreneurial: u8,
    /// Drives a future `SocializeScorer` multiplier. Gregarious
    /// agents seek social interaction faster than the Needs-driven
    /// threshold would suggest alone.
    pub gregariousness: u8,
    /// Drives a future `SelfActualizationScorer` for teaching /
    /// learning preferences.
    pub curiosity: u8,
    /// Drives a future combat-engagement scorer (independent of the
    /// faction-level `culture.martial` — captures individual variance
    /// within a martial culture).
    pub martial: u8,
}

impl Default for Disposition {
    fn default() -> Self {
        Self {
            entrepreneurial: 128,
            gregariousness: 128,
            curiosity: 128,
            martial: 128,
        }
    }
}

impl Disposition {
    /// Multiplier in `[1.0, 2.0]` consumed by `EarnIncomeScorer`. A
    /// median agent scores at `1.5×`; a maximally-entrepreneurial
    /// agent at `2.0×`; a minimally-entrepreneurial one at `1.0×`.
    pub fn earn_income_multiplier(&self) -> f32 {
        1.0 + self.entrepreneurial as f32 / 255.0
    }
}

/// Tier taxonomy mirroring the plan's `GoalClass` enum. Higher
/// variants win ties between qualitatively different goals — a
/// `Survival`-tier "I'm starving" beats an `Enterprise`-tier
/// "there's a 25-currency Craft posting" every time, regardless of
/// the underlying scores. Within a tier, raw `score` ranks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum GoalClass {
    /// Play, idle, low-priority filler.
    Discretionary = 0,
    /// Paid work, commerce, currency accumulation.
    Enterprise = 1,
    /// Status, prestige, accumulated wealth.
    Esteem = 2,
    /// Socializing, mating, group belonging.
    Belonging = 3,
    /// Raid / defend / rescue — under attack / faction at war.
    Safety = 4,
    /// Food, sleep, shelter — autonomous baseline.
    Subsistence = 5,
    /// Life-or-death — starving, freezing, dying.
    Survival = 6,
}

/// How long a selected autonomous goal should be sticky before normal
/// hysteresis may replace it. Most scorers use `None`; long chains and
/// future institutional commitments can opt into stronger stickiness.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GoalCommitment {
    None,
    UntilTaskComplete,
    UntilTick(u64),
    UntilNeedBelow { need: NeedAxis, threshold: f32 },
}

impl Default for GoalCommitment {
    fn default() -> Self {
        Self::None
    }
}

/// Need axis used by `GoalCommitment::UntilNeedBelow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedAxis {
    Hunger,
    Thirst,
    Sleep,
    Social,
    Willpower,
    Safety,
    Esteem,
}

/// Shared interrupt contract for normal goal re-evaluation and
/// opportunistic mid-walk interruptions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptPolicy {
    AlwaysInterruptible,
    InterruptibleByHigherClass,
    UninterruptibleExceptSurvival,
}

impl Default for InterruptPolicy {
    fn default() -> Self {
        Self::AlwaysInterruptible
    }
}

pub fn default_class_for_goal(goal: AgentGoal) -> GoalClass {
    match goal {
        AgentGoal::Survive | AgentGoal::Sleep | AgentGoal::SeekCare | AgentGoal::Drink => {
            GoalClass::Survival
        }
        AgentGoal::Raid
        | AgentGoal::Defend
        | AgentGoal::Rescue
        | AgentGoal::FollowingPlayerCommand => GoalClass::Safety,
        AgentGoal::Socialize => GoalClass::Belonging,
        AgentGoal::Build | AgentGoal::Lead => GoalClass::Esteem,
        AgentGoal::Play => GoalClass::Discretionary,
        AgentGoal::GatherFood
        | AgentGoal::GatherWood
        | AgentGoal::GatherStone
        | AgentGoal::ReturnCamp
        | AgentGoal::TameAnimal
        | AgentGoal::Craft
        | AgentGoal::Farm
        | AgentGoal::Haul
        | AgentGoal::Stockpile
        | AgentGoal::MigrateToCamp
        | AgentGoal::Scout
        | AgentGoal::ProvideCare => GoalClass::Subsistence,
    }
}

pub fn default_interrupt_policy_for_goal(goal: AgentGoal) -> InterruptPolicy {
    match goal {
        AgentGoal::Survive
        | AgentGoal::Sleep
        | AgentGoal::Raid
        | AgentGoal::Defend
        | AgentGoal::Rescue
        | AgentGoal::FollowingPlayerCommand
        | AgentGoal::SeekCare
        | AgentGoal::Drink => InterruptPolicy::UninterruptibleExceptSurvival,
        _ => InterruptPolicy::AlwaysInterruptible,
    }
}

pub fn interrupt_policy_allows(
    policy: InterruptPolicy,
    current_class: GoalClass,
    challenger_class: GoalClass,
) -> bool {
    match policy {
        InterruptPolicy::AlwaysInterruptible => true,
        InterruptPolicy::InterruptibleByHigherClass => challenger_class > current_class,
        InterruptPolicy::UninterruptibleExceptSurvival => challenger_class == GoalClass::Survival,
    }
}

/// What a scorer returns when it has an opinion. `None` means the
/// scorer declines to set a goal for this agent (e.g. an
/// `EarnIncomeScorer` returns `None` when the agent has no
/// profession). The dispatcher argmaxes over `(class, score)`.
#[derive(Clone, Copy, Debug)]
pub struct GoalScore {
    pub goal: AgentGoal,
    pub class: GoalClass,
    pub score: f32,
    pub reason: &'static str,
    pub commitment: GoalCommitment,
    pub interrupt_policy: InterruptPolicy,
}

impl GoalScore {
    pub fn new(goal: AgentGoal, class: GoalClass, score: f32, reason: &'static str) -> Self {
        Self {
            goal,
            class,
            score,
            reason,
            commitment: GoalCommitment::None,
            interrupt_policy: InterruptPolicy::default(),
        }
    }

    pub fn with_commitment(mut self, commitment: GoalCommitment) -> Self {
        self.commitment = commitment;
        self
    }

    pub fn with_interrupt_policy(mut self, interrupt_policy: InterruptPolicy) -> Self {
        self.interrupt_policy = interrupt_policy;
        self
    }
}

/// Per-agent decision trace. This is intentionally compact and copyable:
/// the debug UI can read it without chasing scorer objects, and the goal
/// systems can use it for commitment/interrupt policy without rebuilding
/// the full scoring context.
#[derive(Component, Clone, Copy, Debug)]
pub struct AgentDecisionState {
    pub last_goal: AgentGoal,
    pub last_class: GoalClass,
    pub last_score: f32,
    pub last_reason: &'static str,
    pub last_scorer: &'static str,
    pub last_evaluation_tick: u64,
    pub commitment: GoalCommitment,
    pub commitment_expires_tick: Option<u64>,
    pub interrupt_policy: InterruptPolicy,
}

impl Default for AgentDecisionState {
    fn default() -> Self {
        Self {
            last_goal: AgentGoal::GatherFood,
            last_class: GoalClass::Discretionary,
            last_score: 0.0,
            last_reason: "",
            last_scorer: "",
            last_evaluation_tick: 0,
            commitment: GoalCommitment::None,
            commitment_expires_tick: None,
            interrupt_policy: InterruptPolicy::default(),
        }
    }
}

impl AgentDecisionState {
    pub fn record_score(&mut self, score: GoalScore, scorer_name: &'static str, now: u64) {
        self.last_goal = score.goal;
        self.last_class = score.class;
        self.last_score = score.score;
        self.last_reason = score.reason;
        self.last_scorer = scorer_name;
        self.last_evaluation_tick = now;
        self.commitment = score.commitment;
        self.commitment_expires_tick = match score.commitment {
            GoalCommitment::UntilTick(tick) => Some(tick),
            _ => None,
        };
        self.interrupt_policy = score.interrupt_policy;
    }

    pub fn record_forced(
        &mut self,
        goal: AgentGoal,
        class: GoalClass,
        reason: &'static str,
        now: u64,
        interrupt_policy: InterruptPolicy,
    ) {
        self.last_goal = goal;
        self.last_class = class;
        self.last_score = 1.0;
        self.last_reason = reason;
        self.last_scorer = "Forced";
        self.last_evaluation_tick = now;
        self.commitment = GoalCommitment::UntilTaskComplete;
        self.commitment_expires_tick = None;
        self.interrupt_policy = interrupt_policy;
    }
}

pub const AGENT_GOAL_COUNT: usize = 24;

/// Lightweight counters for profiling the decision pipeline without
/// pulling in a benchmarking dependency.
#[derive(Resource, Clone, Debug)]
pub struct DecisionMetrics {
    pub goal_evaluations: u64,
    pub scorer_evaluations: u64,
    pub chosen_goal_counts: [u64; AGENT_GOAL_COUNT],
    pub htn_method_attempts: u64,
    pub htn_method_successes: u64,
    pub htn_method_failures: u64,
    pub action_queue_samples: u64,
    pub action_queue_total_len: u64,
    pub lod_full: u32,
    pub lod_aggregate: u32,
    pub lod_dormant: u32,
    pub last_sample_tick: u64,
}

impl Default for DecisionMetrics {
    fn default() -> Self {
        Self {
            goal_evaluations: 0,
            scorer_evaluations: 0,
            chosen_goal_counts: [0; AGENT_GOAL_COUNT],
            htn_method_attempts: 0,
            htn_method_successes: 0,
            htn_method_failures: 0,
            action_queue_samples: 0,
            action_queue_total_len: 0,
            lod_full: 0,
            lod_aggregate: 0,
            lod_dormant: 0,
            last_sample_tick: 0,
        }
    }
}

impl DecisionMetrics {
    pub fn record_goal_pick(&mut self, goal: AgentGoal) {
        let idx = goal as usize;
        if idx < AGENT_GOAL_COUNT {
            self.chosen_goal_counts[idx] = self.chosen_goal_counts[idx].saturating_add(1);
        }
    }

    pub fn average_action_queue_len(&self) -> f32 {
        if self.action_queue_samples == 0 {
            0.0
        } else {
            self.action_queue_total_len as f32 / self.action_queue_samples as f32
        }
    }
}

/// Read-only per-agent context the registry passes to each scorer.
/// Anything a scorer might legitimately want — needs, skills,
/// profession, disposition, faction view, job board, tile, current
/// tick — lives here so individual scorers stay one-function pure
/// reads.
pub struct GoalScoringContext<'a> {
    pub agent: Entity,
    pub agent_tile: (i32, i32),
    pub now: u64,
    pub needs: &'a Needs,
    pub profession: Profession,
    pub skills: &'a Skills,
    pub disposition: Disposition,
    pub economic_agent: &'a EconomicAgent,
    pub faction_member: &'a FactionMember,
    pub faction: &'a FactionData,
    pub board: &'a JobBoard,
    pub opportunities: Option<&'a crate::simulation::opportunity::OpportunityIndex>,
    // ── Phase B (behavioural richness): precomputed gates ──────────
    // `goal_update_system` already derives these per agent each tick;
    // hoisting them into the context lets scorers stay pure-read.
    pub is_starving: bool,
    pub faction_has_food: bool,
    pub can_return_camp: bool,
    pub prioritize_food: bool,
    pub fallback_gather: AgentGoal,
    pub fallback_gather_reason: &'static str,
    /// Generalised taming awareness. True when the faction is Aware of any
    /// taming tech (HORSE_TAMING / ANIMAL_HUSBANDRY / DOG_DOMESTICATION) AND a
    /// matching wild candidate exists in the world. The dispatcher does the
    /// per-species reconciliation at scan time.
    pub has_tameable_animal: bool,
    pub has_personal_build_site: bool,
    pub should_craft: bool,
    /// Heal-pipeline (Heal-2): the agent's own injury, if any. Read
    /// by `HealNeedScorer` to gate `SeekCare` and drive
    /// severity-weighted urgency.
    pub injury: Option<crate::simulation::medicine::Injury>,
    /// Heal-pipeline (Heal-2): true when any agent in the same
    /// faction currently carries an `Injury` component. Read by
    /// `ProvideCareScorer` (Healer-side) to gate `ProvideCare`.
    pub faction_has_injured: bool,
    /// Farming: true when the agent is a `HouseholdMember` of a household
    /// that holds a `ZoneKind::Agricultural` plot (with state-Owned or
    /// household-Owned tenure). Read by `FarmWorkScorer` so any
    /// household adult — not only `Profession::Farmer` — can nominate
    /// `AgentGoal::Farm` to work the household's plot.
    pub private_farm_available: bool,
    /// Seasonal-farming jellyfish: the current `FarmSeasonPhase`. Read by
    /// `FarmWorkScorer` to gate `AgentGoal::Farm` (Winter ⇒ never fires).
    pub farm_season: crate::simulation::farm::FarmSeasonPhase,
    /// Seasonal-farming jellyfish: true when the agent's household plot has
    /// at least one tile that needs Prepare/Plant (Spring) or Harvest
    /// (Autumn) work. Computed once per goal-update tick into a snapshot
    /// (mirrors `private_farm_available`).
    pub private_plot_has_seasonal_work: bool,
    /// 0.0 daytime → 1.0 night; lifts `SleepScorer`'s curve so a
    /// moderately-tired agent picks sleep over work after dusk.
    pub time_of_day_bonus: f32,
    /// Agent age in ticks. Today the sim doesn't track per-person age,
    /// so callers pass an "adult" default (~5 game-years) and
    /// `PlayScorer`'s youth-falloff degrades all agents uniformly.
    /// When age tracking lands, scorer behaviour shifts without
    /// touching the scorer code.
    pub age_ticks: u64,
}

/// Trait every scorer implements. Trait objects live in the
/// `GoalScorerRegistry` so the dispatcher can iterate them without
/// hardcoding the list. Adding a new scorer = new struct + `impl
/// GoalScorer` + one `registry.scorers.push(Box::new(...))` in plugin
/// setup.
pub trait GoalScorer: Send + Sync + 'static {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore>;
    /// Stable debug name surfaced by the inspector / activity log.
    fn name(&self) -> &'static str;
    /// Phase D: true when this scorer is eligible to *preempt* an
    /// en-route agent mid-walk via `opportunistic_interrupt_system`.
    /// Default `false` — only the cheap-detour scorers
    /// (`SocialScorer`, `PlayScorer`) opt in. Survival/Subsistence
    /// scorers don't need this — they preempt via the normal
    /// `goal_update_system` cadence using class precedence.
    fn opportunistic(&self) -> bool {
        false
    }
}

/// Resource: the live list of scorers. Built once at
/// `SimulationPlugin::build`; queried per-agent per-tick.
///
/// `opportunistic_indices` is a cached projection of
/// `scorers.iter().enumerate().filter(|s| s.opportunistic())` —
/// `opportunistic_interrupt_system` runs every 20 ticks across every
/// walking agent and would otherwise burn ~80% of its scorer walk
/// rejecting non-opportunistic candidates. Rebuild via
/// `rebuild_opportunistic_indices()` after mutating `scorers`.
#[derive(Resource, Default)]
pub struct GoalScorerRegistry {
    pub scorers: Vec<Box<dyn GoalScorer>>,
    pub opportunistic_indices: Vec<usize>,
}

impl GoalScorerRegistry {
    /// Argmax over every scorer for one agent. Returns `None` when
    /// every scorer declines (e.g. all gates failed). Ties broken by
    /// `class` first (higher wins), then `score` within a class.
    pub fn best(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        self.best_with_incumbent(ctx, None).0.map(|pick| pick.score)
    }

    /// Single-pass `best` + incumbent-score lookup. When
    /// `current_goal == Some(g)`, also returns the highest-class /
    /// highest-score `GoalScore` whose `goal == g`, capped to
    /// `class >= best.class` so the caller can run a hysteresis
    /// comparison without a second scorer walk. Eliminates the
    /// double-scoring `goal_update_system::Scored` did when the new
    /// pick differed from the agent's current goal.
    pub fn best_with_incumbent(
        &self,
        ctx: &GoalScoringContext,
        current_goal: Option<AgentGoal>,
    ) -> (Option<GoalScorerPick>, Option<f32>) {
        let mut best: Option<GoalScorerPick> = None;
        let mut incumbent: Option<GoalScore> = None;
        for scorer in &self.scorers {
            let Some(candidate) = scorer.score(ctx) else {
                continue;
            };
            if best.map_or(true, |b| Self::beats(candidate, b.score)) {
                best = Some(GoalScorerPick {
                    score: candidate,
                    scorer_name: scorer.name(),
                });
            }
            if current_goal == Some(candidate.goal)
                && incumbent.map_or(true, |i| Self::beats(candidate, i))
            {
                incumbent = Some(candidate);
            }
        }
        let incumbent_score = match (best, incumbent) {
            (Some(b), Some(i)) if i.class >= b.score.class => Some(i.score),
            _ => None,
        };
        (best, incumbent_score)
    }

    /// Strict greater-than under the (class, score) lex order used by
    /// `best`. Pulled out so `best_with_incumbent`'s two argmax tracks
    /// share one comparison.
    #[inline]
    fn beats(candidate: GoalScore, current: GoalScore) -> bool {
        match candidate.class.cmp(&current.class) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => candidate.score > current.score,
        }
    }

    pub fn rebuild_opportunistic_indices(&mut self) {
        self.opportunistic_indices = self
            .scorers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.opportunistic().then_some(i))
            .collect();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GoalScorerPick {
    pub score: GoalScore,
    pub scorer_name: &'static str,
}

// ─── EarnIncomeScorer ──────────────────────────────────────────────

/// Phase 6's headline scorer. Walks the agent's faction `JobBoard`
/// for unclaimed paid postings whose `JobKind` matches the agent's
/// profession's `job_kinds_for(...)` set. Scores the best candidate
/// as `posting.reward × skill_competence(primary_skill) ×
/// disposition.earn_income_multiplier()`. Travel cost is intentionally
/// left to `job_claim_system`'s `U_bid` at claim time (claim layer
/// handles distance; goal layer stays coarse).
///
/// Returns `None` (declines) when:
/// - Agent is `Profession::None / Apprentice`. Idle Nones drift on
///   subsistence reflexes; Apprentices route earnings via their
///   mentor's posting wage-split.
/// - Faction is Subsistence (empty `economic_policy` AND empty
///   `wage_signal`). Communal labor untouched.
/// - No matching unclaimed paid posting exists.
///
/// Tier = `Enterprise`. Outranks `Discretionary` (play / idle) and
/// loses to every higher tier — a hungry / sleeping / under-raid
/// agent ignores income opportunities.
pub struct EarnIncomeScorer;

impl GoalScorer for EarnIncomeScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        use crate::simulation::profession_choice::{
            job_kinds_for, primary_skill_for, skill_competence,
        };
        if matches!(ctx.profession, Profession::None | Profession::Apprentice) {
            return None;
        }
        if ctx.faction.economic_policy.is_empty() && ctx.faction.wage_signal.is_empty() {
            return None;
        }
        let kinds = job_kinds_for(ctx.profession);
        if kinds.is_empty() {
            return None;
        }
        let comp = primary_skill_for(ctx.profession)
            .map(|k| skill_competence(ctx.skills.get(k)))
            .unwrap_or(1.0);
        let mult = ctx.disposition.earn_income_multiplier();
        let mut best: Option<(JobKind, f32)> = None;
        if let Some(opportunities) = ctx.opportunities {
            for opportunity in opportunities.iter_kind_for_faction(
                ctx.faction_member.faction_id,
                crate::simulation::opportunity::OpportunityKind::PaidJob,
            ) {
                let crate::simulation::opportunity::OpportunityPayload::PaidJob {
                    kind,
                    reward,
                    ..
                } = opportunity.payload
                else {
                    continue;
                };
                if !kinds.contains(&kind) {
                    continue;
                }
                let score = reward * comp * mult;
                if best.map(|(_, s)| score > s).unwrap_or(true) {
                    best = Some((kind, score));
                }
            }
        }
        if best.is_none() {
            for posting in ctx.board.faction_postings(ctx.faction_member.faction_id) {
                if posting.reward <= 0.0 {
                    continue;
                }
                if !kinds.contains(&posting.kind) {
                    continue;
                }
                if !posting.claimants.is_empty() {
                    continue;
                }
                let score = posting.reward * comp * mult;
                if best.map(|(_, s)| score > s).unwrap_or(true) {
                    best = Some((posting.kind, score));
                }
            }
        }
        let (kind, score) = best?;
        Some(GoalScore::new(
            kind.to_goal(),
            GoalClass::Enterprise,
            score,
            "Earning Income",
        ))
    }

    fn name(&self) -> &'static str {
        "EarnIncomeScorer"
    }
}

// ─── Phase B scorers (behavioural richness) ────────────────────────
//
// These migrate the need-driven branches of `goal_update_system`'s
// imperative cascade onto continuous utility curves. As of Phase F-2
// the scorer pipeline is the only path through `goal_update_system`;
// the historical Legacy cascade survives only as a SOLO / faction-
// lookup-miss fallback (`fallback_pick` inside the system).
//
// Per-scorer Disposition multipliers (max lifts):
const SOCIAL_GREG_LIFT: f32 = 1.5;
const PLAY_GREG_LIFT: f32 = 1.2;

/// Survival-class hunger scorer. Replaces the four `Survive` branches
/// in the legacy cascade with one continuous curve. Distinguishes
/// "eat what you hold" from "go forage" via `EconomicAgent.total_food`
/// and `faction_has_food` so the rendered `GoalReason` matches the
/// legacy text the inspector and tests rely on.
pub struct SurvivalHungerScorer;

impl GoalScorer for SurvivalHungerScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let urgency = hunger_utility(ctx.needs.hunger);
        if urgency < 0.10 {
            return None;
        }
        let has_food = ctx.economic_agent.total_food() > 0;
        let reason: &'static str = if ctx.is_starving && ctx.faction_has_food {
            "Starving (Faction has food)"
        } else if ctx.needs.hunger > 200.0 && !has_food {
            "Very Hungry"
        } else if ctx.needs.hunger > 180.0 && has_food {
            "Hungry (Eating)"
        } else if ctx.needs.hunger >= crate::simulation::needs::EAT_TRIGGER_HUNGER as f32
            && !has_food
        {
            "Hungry"
        } else {
            // Below the dispatcher gate (EAT_TRIGGER_HUNGER, 180) no HTN
            // food/eat method's precondition matches and both food
            // dispatchers early-return. Emitting Survive here would pin
            // the agent on a Survival-class, work-preempting goal that
            // can't decompose into a task. Fall through to lower-class
            // scorers instead — sub-180 hunger prep is faction/economy
            // scope (GatherFood / Stockpile postings), not personal
            // Survive.
            return None;
        };
        Some(
            GoalScore::new(AgentGoal::Survive, GoalClass::Survival, urgency, reason)
                .with_commitment(GoalCommitment::UntilNeedBelow {
                    need: NeedAxis::Hunger,
                    threshold: 120.0,
                })
                .with_interrupt_policy(InterruptPolicy::UninterruptibleExceptSurvival),
        )
    }

    fn name(&self) -> &'static str {
        "SurvivalHungerScorer"
    }
}

/// Survival-class thirst scorer. Mirrors `SurvivalHungerScorer` but reads
/// `needs.thirst` against `THIRST_TRIGGER` / `THIRST_SEVERE`. Lower-class
/// than hunger only via the score curve — both live in `Survival` class so
/// a parched, hungry agent picks the more urgent need by raw urgency.
pub struct ThirstScorer;

impl GoalScorer for ThirstScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let t = ctx.needs.thirst;
        // Task-side gate: dispatcher refuses Drink below THIRST_TRIGGER, so
        // scoring it here would pin a goal that can't decompose.
        if t < crate::simulation::needs::THIRST_TRIGGER {
            return None;
        }
        // Two-stage smoothstep mirroring hunger but tilted higher — dehydration
        // kills faster than starvation, so a parched agent should outrank an
        // equally hungry one. Hits ~0.60 at TRIGGER (180), ~0.95 at SEVERE (230).
        let urgency = thirst_utility(t);
        if urgency < 0.10 {
            return None;
        }
        let reason: &'static str = if t >= crate::simulation::needs::THIRST_SEVERE {
            "Parched"
        } else {
            "Thirsty"
        };
        Some(
            GoalScore::new(AgentGoal::Drink, GoalClass::Survival, urgency, reason)
                .with_commitment(GoalCommitment::UntilNeedBelow {
                    need: NeedAxis::Thirst,
                    threshold: 120.0,
                })
                .with_interrupt_policy(InterruptPolicy::UninterruptibleExceptSurvival),
        )
    }

    fn name(&self) -> &'static str {
        "ThirstScorer"
    }
}

/// Survival-class sleep scorer. Reads `time_of_day_bonus` to lift
/// urgency after dusk so a moderately tired agent picks sleep over
/// work at night even when `sleep < SLEEP_TIRED`.
pub struct SleepScorer;

impl GoalScorer for SleepScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let urgency = sleep_utility(ctx.needs.sleep, ctx.time_of_day_bonus);
        if urgency < 0.15 {
            return None;
        }
        Some(
            GoalScore::new(AgentGoal::Sleep, GoalClass::Survival, urgency, "Tired")
                .with_commitment(GoalCommitment::UntilNeedBelow {
                    need: NeedAxis::Sleep,
                    threshold: 80.0,
                })
                .with_interrupt_policy(InterruptPolicy::UninterruptibleExceptSurvival),
        )
    }

    fn name(&self) -> &'static str {
        "SleepScorer"
    }
}

/// Subsistence-class scorer for "I'm carrying food, head home." Only
/// fires when `can_return_camp` is true (faction has reachable
/// storage AND stock is below cap), matching the legacy gate.
pub struct ReturnSurplusScorer;

impl GoalScorer for ReturnSurplusScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !ctx.can_return_camp {
            return None;
        }
        let food = ctx.economic_agent.total_food();
        if food < 3 {
            return None;
        }
        // Held-food count drives urgency; gregarious agents return
        // home (where the people are) a touch sooner.
        let base = ((food as f32 - 3.0) / 6.0).clamp(0.0, 1.0);
        let lift = disposition_lift(ctx.disposition.gregariousness, 0.1);
        Some(GoalScore::new(
            AgentGoal::ReturnCamp,
            GoalClass::Subsistence,
            (base * lift).clamp(0.0, 1.0),
            "Returning Surplus Food",
        ))
    }

    fn name(&self) -> &'static str {
        "ReturnSurplusScorer"
    }
}

/// Belonging-class social scorer. Modulated by gregariousness — a
/// gregarious agent feels lonely at lower `needs.social`, a loner
/// shrugs it off longer (see `utility_curves::social_utility`).
pub struct SocialScorer;

impl GoalScorer for SocialScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let urgency = social_utility(ctx.needs.social, ctx.disposition.gregariousness);
        if urgency < 0.15 {
            return None;
        }
        let lift = disposition_lift(ctx.disposition.gregariousness, SOCIAL_GREG_LIFT);
        Some(GoalScore::new(
            AgentGoal::Socialize,
            GoalClass::Belonging,
            (urgency * lift).clamp(0.0, 1.0),
            "Social Need",
        ))
    }

    fn name(&self) -> &'static str {
        "SocialScorer"
    }

    fn opportunistic(&self) -> bool {
        true
    }
}

/// Discretionary-class play scorer. Lowest tier — only wins when
/// every other need is satisfied. Loners still play (solo throw /
/// solo plant) but gregarious agents play more.
pub struct PlayScorer;

impl GoalScorer for PlayScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let urgency = play_utility(
            ctx.needs.willpower,
            ctx.age_ticks,
            ctx.disposition.gregariousness,
        );
        if urgency < 0.20 {
            return None;
        }
        let lift = disposition_lift(ctx.disposition.gregariousness, PLAY_GREG_LIFT);
        Some(GoalScore::new(
            AgentGoal::Play,
            GoalClass::Discretionary,
            (urgency * lift).clamp(0.0, 1.0),
            "Low Willpower",
        ))
    }

    fn name(&self) -> &'static str {
        "PlayScorer"
    }

    fn opportunistic(&self) -> bool {
        true
    }
}

/// Subsistence-class scorer for wild-animal taming. Gated on the
/// precomputed `has_tameable_animal` flag (faction Aware of HORSE_TAMING
/// / ANIMAL_HUSBANDRY / DOG_DOMESTICATION + at least one matching wild
/// candidate in range). Curiosity Disposition lifts the score so curious
/// agents grab the opportunity over crafting. The dispatcher resolves
/// the actual species per agent at scan time.
pub struct TameAnimalScorer;

impl GoalScorer for TameAnimalScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !ctx.has_tameable_animal {
            return None;
        }
        let lift = disposition_lift(ctx.disposition.curiosity, 0.5);
        Some(GoalScore::new(
            AgentGoal::TameAnimal,
            GoalClass::Subsistence,
            (0.45 * lift).clamp(0.0, 1.0),
            "Taming Animal",
        ))
    }

    fn name(&self) -> &'static str {
        "TameAnimalScorer"
    }
}

/// Esteem-class scorer for personal-build projects (player-commissioned
/// blueprints owned by the agent). Bypasses the faction job board —
/// this is the agent's own build, not communal labor.
pub struct PersonalBuildScorer;

impl GoalScorer for PersonalBuildScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !ctx.has_personal_build_site {
            return None;
        }
        Some(GoalScore::new(
            AgentGoal::Build,
            GoalClass::Esteem,
            0.80,
            "Building Personal Project",
        ))
    }

    fn name(&self) -> &'static str {
        "PersonalBuildScorer"
    }
}

/// Subsistence-class scorer for faction-driven crafting. Gated on the
/// precomputed `should_craft` flag (faction has craft tech + low
/// crafted-good stock + recipe inputs available + agent not under
/// craft cooldown). Entrepreneurial agents lift the score so they
/// prefer paid craft work over generic gather.
pub struct CraftDemandScorer;

impl GoalScorer for CraftDemandScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !ctx.should_craft {
            return None;
        }
        // Entrepreneurial Crafters chase craft demand more eagerly.
        let prof_bonus = matches!(ctx.profession, Profession::Crafter | Profession::Apprentice);
        let lift = disposition_lift(ctx.disposition.entrepreneurial, 0.5)
            * if prof_bonus { 1.2 } else { 1.0 };
        Some(GoalScore::new(
            AgentGoal::Craft,
            GoalClass::Subsistence,
            (0.50 * lift).clamp(0.0, 1.0),
            "Crafting for Faction",
        ))
    }

    fn name(&self) -> &'static str {
        "CraftDemandScorer"
    }
}

/// Fallback gather scorer: gather food / wood / stone based on
/// faction deficits. Always fires so the scorer pipeline has a
/// guaranteed default goal. **Two-tier**:
/// - `prioritize_food` true → `Subsistence` class with score 0.85, so
///   it preempts every Belonging / Esteem / Enterprise scorer (matches
///   legacy step-7 preempt where a faction food crisis interrupts
///   socializing / play).
/// - otherwise → `Discretionary` class with score 0.20, so it sits at
///   the bottom of the priority stack as the idle-agent default
///   (matches legacy step-13 fallback).
///
/// The class bifurcation is the right knob: priority is a tier
/// decision, not a score decision.
pub struct StockpileScorer;

impl GoalScorer for StockpileScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let (class, score) = if ctx.prioritize_food {
            (GoalClass::Subsistence, 0.85)
        } else {
            (GoalClass::Discretionary, 0.20)
        };
        Some(GoalScore::new(
            ctx.fallback_gather,
            class,
            score,
            ctx.fallback_gather_reason,
        ))
    }

    fn name(&self) -> &'static str {
        "StockpileScorer"
    }
}

/// Heal-pipeline (Heal-2). Survival-class scorer for injured agents.
/// Class is `Survival` (not `Safety` per the plan's first sketch)
/// because the enum's numeric ordering puts `Subsistence > Safety`,
/// so a Safety-tier injury would lose to routine gather/craft. Score
/// scales with `Injury.severity / 255`: a 60/255 wound returns ~0.24,
/// a 200/255 wound returns ~0.78. Combined with class precedence,
/// light injuries naturally lose to acute hunger/sleep within the
/// Survival tier; severe injuries beat them.
pub struct HealNeedScorer;

impl GoalScorer for HealNeedScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let injury = ctx.injury?;
        if injury.severity == 0 {
            return None;
        }
        let urgency = injury.severity as f32 / 255.0;
        Some(
            GoalScore::new(AgentGoal::SeekCare, GoalClass::Survival, urgency, "Injured")
                .with_interrupt_policy(InterruptPolicy::UninterruptibleExceptSurvival),
        )
    }

    fn name(&self) -> &'static str {
        "HealNeedScorer"
    }
}

/// Heal-pipeline (Heal-2). Subsistence-class scorer for Healers (and
/// Healer apprentices) when the faction has at least one injured
/// agent. Score = 0.65 baseline so it beats `CraftDemand` (0.50) and
/// `StockpileScorer`-idle (0.20 Discretionary) — a Healer with
/// patients waiting shouldn't be off gathering wood — but loses to
/// `StockpileScorer`-prioritize-food (0.85) so a Healer in a starving
/// faction still hunts food first. Faction-level gating only;
/// per-patient triage lives in the HTN method (Heal-3) where it can
/// read spatial position.
pub struct ProvideCareScorer;

impl GoalScorer for ProvideCareScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !matches!(ctx.profession, Profession::Healer | Profession::Apprentice) {
            return None;
        }
        let has_care_opportunity = ctx
            .opportunities
            .map(|idx| {
                idx.iter_kind_for_faction(
                    ctx.faction_member.faction_id,
                    crate::simulation::opportunity::OpportunityKind::CareNeed,
                )
                .next()
                .is_some()
            })
            .unwrap_or(ctx.faction_has_injured);
        if !has_care_opportunity {
            return None;
        }
        Some(GoalScore::new(
            AgentGoal::ProvideCare,
            GoalClass::Subsistence,
            0.65,
            "Tending Patient",
        ))
    }

    fn name(&self) -> &'static str {
        "ProvideCareScorer"
    }
}

/// Subsistence-class scorer for private (non-chief-allocated) farming.
/// Fires for any `HouseholdMember` whose household holds an Agricultural
/// plot, when the faction's grain policy permits `private_actors_allowed`
/// (Mixed / Market presets). Farmers are still preferred via the EV/skill
/// path (higher `expected_wage` competence + skill XP) and via a small
/// score lift below; non-Farmer household adults can also nominate Farm
/// so a household with a kitchen garden isn't blocked when its head is a
/// Mason or Apprentice. The actual "do you have seeds / mature crops"
/// check happens at HTN dispatch time.
pub struct FarmWorkScorer;

impl GoalScorer for FarmWorkScorer {
    fn score(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        if !ctx.private_farm_available {
            return None;
        }
        // Seasonal-farming jellyfish: Winter ⇒ no farm goal (eliminates idle
        // mid-Winter farm loops); Spring/Autumn require live seasonal work.
        if matches!(
            ctx.farm_season,
            crate::simulation::farm::FarmSeasonPhase::WinterDormant
        ) {
            return None;
        }
        if !ctx.private_plot_has_seasonal_work {
            return None;
        }
        // Private farming is gated on grain's `private_actors_allowed`.
        // Communal villages still fall through chief Farm postings via the
        // claim system; this scorer is the *self-directed* path.
        let policy = ctx.faction.policy_for(crate::economy::core_ids::grain());
        if !policy.private_actors_allowed {
            return None;
        }
        let base = 0.90;
        let lift = if matches!(ctx.profession, Profession::Farmer) {
            0.05
        } else {
            0.0
        };
        let reason = if lift > 0.0 {
            "Private Farmer Working Plot"
        } else {
            "Household Adult Tending Plot"
        };
        Some(GoalScore::new(
            AgentGoal::Farm,
            GoalClass::Subsistence,
            base + lift,
            reason,
        ))
    }

    fn name(&self) -> &'static str {
        "FarmWorkScorer"
    }
}

/// Convenience: install the default scorer set on a `GoalScorerRegistry`.
/// Phase 6's `EarnIncomeScorer` plus Phase B's behavioural-richness
/// scorers. Consumed by `goal_update_system` (the only goal-selection
/// path as of Phase F-2) and by `earnincome_goal_override_system`.
pub fn register_default_scorers(registry: &mut GoalScorerRegistry) {
    registry.scorers.push(Box::new(EarnIncomeScorer));
    registry.scorers.push(Box::new(SurvivalHungerScorer));
    registry.scorers.push(Box::new(ThirstScorer));
    registry.scorers.push(Box::new(SleepScorer));
    registry.scorers.push(Box::new(ReturnSurplusScorer));
    registry.scorers.push(Box::new(SocialScorer));
    registry.scorers.push(Box::new(PlayScorer));
    registry.scorers.push(Box::new(TameAnimalScorer));
    registry.scorers.push(Box::new(PersonalBuildScorer));
    registry.scorers.push(Box::new(CraftDemandScorer));
    // Farm-planner §10: register before StockpileScorer so a private farmer
    // defaults to working their own plot rather than wandering off to chief
    // postings (survival scorers still preempt via class precedence).
    registry.scorers.push(Box::new(FarmWorkScorer));
    registry.scorers.push(Box::new(StockpileScorer));
    registry.scorers.push(Box::new(HealNeedScorer));
    registry.scorers.push(Box::new(ProvideCareScorer));
    registry.rebuild_opportunistic_indices();
}

pub fn sample_decision_metrics_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mut metrics: ResMut<DecisionMetrics>,
    q: Query<
        (
            &crate::simulation::lod::LodLevel,
            &crate::simulation::typed_task::ActionQueue,
        ),
        With<crate::simulation::person::Person>,
    >,
) {
    if clock.tick % 20 != 0 {
        return;
    }
    metrics.lod_full = 0;
    metrics.lod_aggregate = 0;
    metrics.lod_dormant = 0;
    metrics.action_queue_samples = 0;
    metrics.action_queue_total_len = 0;
    metrics.last_sample_tick = clock.tick;
    for (lod, aq) in q.iter() {
        match *lod {
            crate::simulation::lod::LodLevel::Full => metrics.lod_full += 1,
            crate::simulation::lod::LodLevel::Aggregate => metrics.lod_aggregate += 1,
            crate::simulation::lod::LodLevel::Dormant => metrics.lod_dormant += 1,
        }
        let active: usize = if matches!(aq.current, crate::simulation::typed_task::Task::Idle) {
            0
        } else {
            1
        };
        metrics.action_queue_samples += 1;
        metrics.action_queue_total_len += (active + aq.queued_len()) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::faction::FactionRegistry;

    #[test]
    fn disposition_default_is_median() {
        let d = Disposition::default();
        assert_eq!(d.entrepreneurial, 128);
        assert!((d.earn_income_multiplier() - (1.0 + 128.0 / 255.0)).abs() < 1e-6);
    }

    #[test]
    fn disposition_extremes_bound_multiplier() {
        let lo = Disposition {
            entrepreneurial: 0,
            ..Disposition::default()
        };
        let hi = Disposition {
            entrepreneurial: 255,
            ..Disposition::default()
        };
        assert!((lo.earn_income_multiplier() - 1.0).abs() < 1e-6);
        assert!((hi.earn_income_multiplier() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn goal_class_ordering_matches_plan() {
        assert!(GoalClass::Survival > GoalClass::Subsistence);
        assert!(GoalClass::Subsistence > GoalClass::Safety);
        assert!(GoalClass::Safety > GoalClass::Belonging);
        assert!(GoalClass::Belonging > GoalClass::Esteem);
        assert!(GoalClass::Esteem > GoalClass::Enterprise);
        assert!(GoalClass::Enterprise > GoalClass::Discretionary);
    }

    #[test]
    fn interrupt_policy_contract_matches_classes() {
        assert!(interrupt_policy_allows(
            InterruptPolicy::AlwaysInterruptible,
            GoalClass::Survival,
            GoalClass::Discretionary
        ));
        assert!(interrupt_policy_allows(
            InterruptPolicy::InterruptibleByHigherClass,
            GoalClass::Discretionary,
            GoalClass::Belonging
        ));
        assert!(!interrupt_policy_allows(
            InterruptPolicy::InterruptibleByHigherClass,
            GoalClass::Belonging,
            GoalClass::Discretionary
        ));
        assert!(interrupt_policy_allows(
            InterruptPolicy::UninterruptibleExceptSurvival,
            GoalClass::Subsistence,
            GoalClass::Survival
        ));
        assert!(!interrupt_policy_allows(
            InterruptPolicy::UninterruptibleExceptSurvival,
            GoalClass::Subsistence,
            GoalClass::Belonging
        ));
    }

    #[test]
    fn registry_argmax_breaks_ties_by_class_first() {
        let mut registry = GoalScorerRegistry::default();
        struct StubScorer {
            goal: AgentGoal,
            class: GoalClass,
            score: f32,
        }
        impl GoalScorer for StubScorer {
            fn score(&self, _ctx: &GoalScoringContext) -> Option<GoalScore> {
                Some(GoalScore::new(self.goal, self.class, self.score, "stub"))
            }
            fn name(&self) -> &'static str {
                "Stub"
            }
        }
        // Lower class with massive score loses to higher class with tiny score.
        registry.scorers.push(Box::new(StubScorer {
            goal: AgentGoal::Craft,
            class: GoalClass::Enterprise,
            score: 9999.0,
        }));
        registry.scorers.push(Box::new(StubScorer {
            goal: AgentGoal::Sleep,
            class: GoalClass::Subsistence,
            score: 0.1,
        }));
        let mut reg2 = FactionRegistry::default();
        let fid = reg2.create_faction((0, 0));
        let faction = reg2.factions.get(&fid).unwrap();
        let needs = crate::simulation::needs::Needs::default();
        let skills = Skills::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let ctx = GoalScoringContext {
            agent: Entity::from_raw(0),
            agent_tile: (0, 0),
            now: 0,
            needs: &needs,
            profession: Profession::Crafter,
            skills: &skills,
            disposition: Disposition::default(),
            economic_agent: &agent,
            faction_member: &member,
            faction,
            board: &board,
            opportunities: None,
            is_starving: false,
            faction_has_food: false,
            can_return_camp: false,
            prioritize_food: false,
            fallback_gather: AgentGoal::GatherFood,
            fallback_gather_reason: "stub",
            has_tameable_animal: false,
            has_personal_build_site: false,
            should_craft: false,
            injury: None,
            faction_has_injured: false,
            time_of_day_bonus: 0.0,
            age_ticks: 0,
            private_farm_available: false,
            farm_season: crate::simulation::farm::FarmSeasonPhase::SpringPrepPlant,
            private_plot_has_seasonal_work: false,
        };
        let best = registry.best(&ctx).expect("at least one stub fires");
        assert_eq!(best.goal, AgentGoal::Sleep);
        assert_eq!(best.class, GoalClass::Subsistence);
    }

    // ─── Phase B scorer regression tests ──────────────────────────

    /// Helper to build a minimal context for scorer unit tests.
    fn test_ctx<'a>(
        needs: &'a Needs,
        agent: &'a EconomicAgent,
        member: &'a FactionMember,
        faction: &'a FactionData,
        board: &'a JobBoard,
        skills: &'a Skills,
    ) -> GoalScoringContext<'a> {
        GoalScoringContext {
            agent: Entity::from_raw(0),
            agent_tile: (0, 0),
            now: 0,
            needs,
            profession: Profession::None,
            skills,
            disposition: Disposition::default(),
            economic_agent: agent,
            faction_member: member,
            faction,
            board,
            opportunities: None,
            is_starving: false,
            faction_has_food: false,
            can_return_camp: false,
            prioritize_food: false,
            fallback_gather: AgentGoal::GatherFood,
            fallback_gather_reason: "stub",
            has_tameable_animal: false,
            has_personal_build_site: false,
            should_craft: false,
            injury: None,
            faction_has_injured: false,
            time_of_day_bonus: 0.0,
            age_ticks: crate::simulation::utility_curves::ADULT_AGE_TICKS_PLACEHOLDER,
            private_farm_available: false,
            farm_season: crate::simulation::farm::FarmSeasonPhase::SpringPrepPlant,
            private_plot_has_seasonal_work: false,
        }
    }

    fn make_faction() -> (FactionRegistry, u32) {
        let mut reg = FactionRegistry::default();
        let fid = reg.create_faction((0, 0));
        (reg, fid)
    }

    /// Boundary: empty-handed `Survive` fires exactly at the dispatcher
    /// gate `EAT_TRIGGER_HUNGER (180)` — not below it (where no food task
    /// can decompose), and not just above it.
    #[test]
    fn survival_scorer_fires_at_dispatcher_gate() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();

        // Just below the gate: no Survive — the goal could not decompose.
        let mut needs = Needs::default();
        needs.hunger = 179.0;
        let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        assert!(
            SurvivalHungerScorer.score(&ctx).is_none(),
            "hunger 179 must not emit Survive — below the 180 dispatcher gate",
        );

        // At and above the gate: Survive fires.
        for h in [180.0_f32, 200.0] {
            let mut needs = Needs::default();
            needs.hunger = h;
            let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
            let score = SurvivalHungerScorer
                .score(&ctx)
                .unwrap_or_else(|| panic!("hunger {h} fires"));
            assert_eq!(score.class, GoalClass::Survival);
            assert_eq!(score.goal, AgentGoal::Survive);
            assert!(score.score > 0.4);
        }
    }

    #[test]
    fn survival_scorer_declines_when_sated() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.hunger = 40.0;
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        assert!(SurvivalHungerScorer.score(&ctx).is_none());
    }

    /// Regression: the scorer must NOT emit `Survive` anywhere below the
    /// dispatcher gate `EAT_TRIGGER_HUNGER (180)`. Every HTN food/eat
    /// method gates on `hunger >= 180` and both food dispatchers
    /// early-return below it, so emitting Survive at e.g. hunger 155 pins
    /// the agent on a Survival-class, work-preempting goal that can't
    /// decompose into a task.
    #[test]
    fn survival_scorer_silent_in_pre_cliff_dead_zone() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        for h in [110.0_f32, 120.0, 140.0, 149.0, 155.0, 170.0, 179.0] {
            let mut needs = Needs::default();
            needs.hunger = h;
            let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
            assert!(
                SurvivalHungerScorer.score(&ctx).is_none(),
                "hunger {h} must not emit Survive — no HTN food method matches below 180",
            );
        }
    }

    #[test]
    fn social_scorer_gregariousness_outranks_loner() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.social = 150.0;
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut loner_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        loner_ctx.disposition.gregariousness = 20;
        let mut greg_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        greg_ctx.disposition.gregariousness = 230;
        let loner = SocialScorer.score(&loner_ctx);
        let greg = SocialScorer
            .score(&greg_ctx)
            .expect("gregarious agent fires");
        // Loner may decline entirely at this social level; gregarious agent must fire.
        match loner {
            None => {} // declined — OK, gregarious agent already fires
            Some(l) => assert!(greg.score > l.score, "gregarious must outscore loner"),
        }
    }

    #[test]
    fn return_surplus_requires_camp_reachable() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let mut agent = EconomicAgent::default();
        // 5 units of any food (mocked by hand-inventory check via total_food).
        // EconomicAgent.total_food sums over edible items in inventory; we
        // can't easily seed inventory here without catalog plumbing, so we
        // assert the negative branch.
        let _ = &mut agent;
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.can_return_camp = false;
        assert!(ReturnSurplusScorer.score(&ctx).is_none());
        ctx.can_return_camp = true;
        // total_food still 0 — declines on food check.
        assert!(ReturnSurplusScorer.score(&ctx).is_none());
    }

    #[test]
    fn stockpile_scorer_lifts_when_prioritize_food() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut lo_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        lo_ctx.prioritize_food = false;
        let mut hi_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        hi_ctx.prioritize_food = true;
        let lo = StockpileScorer.score(&lo_ctx).expect("always fires");
        let hi = StockpileScorer.score(&hi_ctx).expect("always fires");
        assert!(hi.score > lo.score);
    }

    #[test]
    fn craft_scorer_only_fires_when_should_craft() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.should_craft = false;
        assert!(CraftDemandScorer.score(&ctx).is_none());
        ctx.should_craft = true;
        let s = CraftDemandScorer
            .score(&ctx)
            .expect("fires when should_craft");
        assert_eq!(s.goal, AgentGoal::Craft);
        assert_eq!(s.class, GoalClass::Subsistence);
    }

    #[test]
    fn tame_horse_scorer_curiosity_lift() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut bored_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        bored_ctx.has_tameable_animal = true;
        bored_ctx.disposition.curiosity = 20;
        let mut curious_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        curious_ctx.has_tameable_animal = true;
        curious_ctx.disposition.curiosity = 230;
        let bored = TameAnimalScorer.score(&bored_ctx).expect("fires");
        let curious = TameAnimalScorer.score(&curious_ctx).expect("fires");
        assert!(curious.score > bored.score);
    }

    #[test]
    fn personal_build_only_fires_when_site_present() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.has_personal_build_site = false;
        assert!(PersonalBuildScorer.score(&ctx).is_none());
        ctx.has_personal_build_site = true;
        let s = PersonalBuildScorer.score(&ctx).expect("fires");
        assert_eq!(s.goal, AgentGoal::Build);
        assert_eq!(s.class, GoalClass::Esteem);
    }

    // ─── Heal-2: HealNeedScorer + ProvideCareScorer ──────────────

    #[test]
    fn heal_need_scorer_declines_when_uninjured() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        assert!(HealNeedScorer.score(&ctx).is_none());
    }

    #[test]
    fn heal_need_scorer_score_scales_with_severity() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut light_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        light_ctx.injury = Some(crate::simulation::medicine::Injury {
            severity: 30,
            applied_tick: 0,
            last_damage_tick: 0,
        });
        let mut severe_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        severe_ctx.injury = Some(crate::simulation::medicine::Injury {
            severity: 200,
            applied_tick: 0,
            last_damage_tick: 0,
        });
        let light = HealNeedScorer
            .score(&light_ctx)
            .expect("light injury fires");
        let severe = HealNeedScorer
            .score(&severe_ctx)
            .expect("severe injury fires");
        assert!(severe.score > light.score);
        assert_eq!(severe.goal, AgentGoal::SeekCare);
        assert_eq!(severe.class, GoalClass::Survival);
    }

    /// Severe injury (Survival class, high score) beats hunger at the
    /// same class via end-to-end registry argmax.
    #[test]
    fn severe_injury_outscores_moderate_hunger() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.hunger = 190.0; // above the 180 gate; SurvivalHungerScorer fires moderately
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.injury = Some(crate::simulation::medicine::Injury {
            severity: 220,
            applied_tick: 0,
            last_damage_tick: 0,
        });
        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);
        let best = registry.best(&ctx).expect("scorer fires");
        assert_eq!(best.goal, AgentGoal::SeekCare);
    }

    #[test]
    fn provide_care_only_fires_for_healers_with_patients() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let needs = Needs::default();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        // Non-Healer: declines regardless of patients.
        ctx.faction_has_injured = true;
        ctx.profession = Profession::Farmer;
        assert!(ProvideCareScorer.score(&ctx).is_none());
        // Healer without patients: declines.
        ctx.profession = Profession::Healer;
        ctx.faction_has_injured = false;
        assert!(ProvideCareScorer.score(&ctx).is_none());
        // Healer with patients: fires.
        ctx.faction_has_injured = true;
        let s = ProvideCareScorer.score(&ctx).expect("fires");
        assert_eq!(s.goal, AgentGoal::ProvideCare);
        assert_eq!(s.class, GoalClass::Subsistence);
    }

    // ─── Phase B-3: end-to-end registry calibration ───────────────

    /// Survival (hunger) at desperation beats any number of competing
    /// Subsistence / Belonging / Esteem scorers — class precedence
    /// alone is sufficient regardless of raw score.
    #[test]
    fn calibration_starving_agent_picks_survive() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.hunger = 220.0; // well above SURVIVE_DESPERATE
        needs.social = 200.0; // tempting socialize
        needs.willpower = 30.0; // tempting play
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.should_craft = true;
        ctx.has_tameable_animal = true;
        ctx.has_personal_build_site = true;

        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);
        let best = registry.best(&ctx).expect("scorer pipeline returns a goal");
        assert_eq!(best.goal, AgentGoal::Survive);
        assert_eq!(best.class, GoalClass::Survival);
    }

    /// Tired agent (no other Survival need) picks Sleep over any
    /// Subsistence work — Sleep is also Survival-class.
    #[test]
    fn calibration_tired_agent_picks_sleep() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.hunger = 50.0;
        needs.sleep = 210.0; // well above SLEEP_TIRED
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        ctx.should_craft = true;

        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);
        let best = registry.best(&ctx).expect("scorer fires");
        assert_eq!(best.goal, AgentGoal::Sleep);
        assert_eq!(best.class, GoalClass::Survival);
    }

    /// With no pressing needs (rested + sated + high willpower), the
    /// StockpileScorer fallback picks `fallback_gather` so the
    /// pipeline always has a default goal. Note: `Needs::default()`
    /// has `willpower = 0.0` (drained, inverted polarity), so the
    /// test must explicitly set it high — otherwise PlayScorer fires.
    #[test]
    fn calibration_idle_agent_falls_back_to_gather() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.willpower = 200.0; // rested, no play urge
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);

        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);
        let best = registry
            .best(&ctx)
            .expect("at minimum StockpileScorer fires");
        assert_eq!(best.goal, AgentGoal::GatherFood);
        // Idle fallback sits at Discretionary, the lowest priority
        // tier, so any higher-need scorer naturally preempts it.
        assert_eq!(best.class, GoalClass::Discretionary);
    }

    /// Behavioural richness payoff: two agents with same needs but
    /// different gregariousness diverge in goal choice.
    ///
    /// Anchor `needs.social = 130` so the loner's inflection (lo=138)
    /// sits above it (SocialScorer declines) but the gregarious
    /// agent's inflection (lo=102) sits below it (SocialScorer fires
    /// at Belonging tier). Loner falls through to StockpileScorer.
    /// `needs.willpower = 200` keeps PlayScorer out of the picture.
    #[test]
    fn calibration_disposition_diverges_goal_choice() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.social = 130.0;
        needs.willpower = 200.0;
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut loner = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        loner.disposition.gregariousness = 10;
        let mut greg = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        greg.disposition.gregariousness = 245;

        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);
        let loner_best = registry.best(&loner).expect("scorer fires");
        let greg_best = registry.best(&greg).expect("scorer fires");
        assert_ne!(
            loner_best.goal, greg_best.goal,
            "different dispositions must yield different goals at same needs (loner={:?} vs greg={:?})",
            loner_best.goal, greg_best.goal,
        );
        assert_eq!(greg_best.goal, AgentGoal::Socialize);
        assert_eq!(loner_best.goal, AgentGoal::GatherFood);
    }

    /// Phase C sweep: across a grid of `(social, hunger, willpower)`
    /// inputs, two agents differing only in `gregariousness` must
    /// pick different goals on **at least some** of the grid points.
    /// 0% divergence would mean Disposition doesn't move the needle;
    /// 100% would mean we've broken the survival precedence. Real
    /// expected behaviour sits between — Survival-class hunger
    /// dominates for high-hunger tuples (no divergence there);
    /// Belonging-class social diverges for mid-social tuples; idle
    /// fallback is unaffected (no divergence there). Target: ≥ 10%
    /// of the grid diverges.
    #[test]
    fn sweep_disposition_drives_visible_divergence() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);

        let mut grid_points = 0usize;
        let mut divergences = 0usize;
        // Grid spans the social_utility inflection band where loner
        // and gregarious agents diverge most. SocialScorer's lo
        // anchors are ~138 (greg=20) vs ~109 (greg=220), so the
        // divergence band sits roughly at `social ∈ [109, 138]`. The
        // grid samples that band plus the boundaries on either side.
        for hunger in [40.0, 80.0, 120.0, 175.0, 210.0] {
            for social in [40.0, 80.0, 110.0, 130.0, 160.0, 200.0] {
                for willpower in [60.0, 140.0, 220.0] {
                    grid_points += 1;
                    let mut needs = Needs::default();
                    needs.hunger = hunger;
                    needs.social = social;
                    needs.willpower = willpower;
                    let mut loner = test_ctx(&needs, &agent, &member, faction, &board, &skills);
                    loner.disposition.gregariousness = 20;
                    let mut greg = test_ctx(&needs, &agent, &member, faction, &board, &skills);
                    greg.disposition.gregariousness = 220;
                    let loner_goal = registry
                        .best(&loner)
                        .expect("at minimum StockpileScorer fires")
                        .goal;
                    let greg_goal = registry
                        .best(&greg)
                        .expect("at minimum StockpileScorer fires")
                        .goal;
                    if loner_goal != greg_goal {
                        divergences += 1;
                    }
                }
            }
        }
        let frac = divergences as f32 / grid_points as f32;
        assert!(
            frac >= 0.10,
            "expected ≥10% disposition-driven divergence across the {grid_points}-point grid, got {divergences} ({:.1}%)",
            frac * 100.0
        );
    }

    /// Survival precedence holds even after Phase B/C wiring: across
    /// the grid, every (hunger ≥ EAT_TRIGGER_HUNGER, no held food) tuple
    /// must pick `Survive` regardless of every other axis.
    #[test]
    fn sweep_survival_precedence_is_total() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);

        for hunger in [180.0, 200.0, 210.0] {
            for social in [50.0, 220.0] {
                for greg in [10_u8, 240] {
                    let mut needs = Needs::default();
                    needs.hunger = hunger;
                    needs.social = social;
                    needs.willpower = 100.0;
                    let mut ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
                    ctx.disposition.gregariousness = greg;
                    let pick = registry.best(&ctx).expect("scorer fires");
                    assert_eq!(
                        pick.goal,
                        AgentGoal::Survive,
                        "hunger={hunger} social={social} greg={greg} must Survive — got {:?}",
                        pick.goal
                    );
                    assert_eq!(pick.class, GoalClass::Survival);
                }
            }
        }
    }

    /// Claimed-worker protection: in the 150–180 band an empty-handed
    /// agent must NOT surface a Survival-class `Survive` goal. `Survive`
    /// is `UninterruptibleExceptSurvival` and preempts claimed work; a
    /// worker re-evaluating goals at hunger 175 would otherwise drop its
    /// job for a goal that can't decompose into a task. Survive may only
    /// preempt at or above the 180 dispatcher gate.
    #[test]
    fn no_survival_preempt_below_dispatcher_gate() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut registry = GoalScorerRegistry::default();
        register_default_scorers(&mut registry);

        for hunger in [155.0_f32, 170.0, 179.0] {
            let mut needs = Needs::default();
            needs.hunger = hunger;
            let ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
            if let Some(pick) = registry.best(&ctx) {
                assert_ne!(
                    pick.class,
                    GoalClass::Survival,
                    "hunger {hunger}: no Survival-class goal may preempt below the 180 gate — got {:?}",
                    pick.goal,
                );
            }
        }
    }

    #[test]
    fn sleep_scorer_night_bonus_lifts_score() {
        let (reg, fid) = make_faction();
        let faction = reg.factions.get(&fid).unwrap();
        let mut needs = Needs::default();
        needs.sleep = 170.0;
        let agent = EconomicAgent::default();
        let member = FactionMember {
            faction_id: fid,
            ..Default::default()
        };
        let board = JobBoard::default();
        let skills = Skills::default();
        let mut day_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        day_ctx.time_of_day_bonus = 0.0;
        let mut night_ctx = test_ctx(&needs, &agent, &member, faction, &board, &skills);
        night_ctx.time_of_day_bonus = 1.0;
        let day = SleepScorer
            .score(&day_ctx)
            .expect("daytime sleep at 170 fires");
        let night = SleepScorer
            .score(&night_ctx)
            .expect("nighttime sleep at 170 fires");
        assert!(night.score > day.score, "night must outscore day");
        assert_eq!(day.class, GoalClass::Survival);
    }
}
