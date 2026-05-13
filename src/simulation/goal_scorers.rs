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
//! The registry is consumed by `goals::earnincome_goal_override_system`
//! (renamed from the Phase 6 procedural fold-in once it switches to
//! reading scorers) — at most one scorer wins per agent per tick, and
//! the existing legacy cascade in `goal_update_system` still drives
//! `Survival` / `Subsistence` / `Safety` branches that this module
//! intentionally doesn't try to migrate (those are correct as-is and
//! migrating them all at once is a much larger refactor).

use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::simulation::faction::{FactionData, FactionMember};
use crate::simulation::goals::AgentGoal;
use crate::simulation::jobs::{JobBoard, JobKind};
use crate::simulation::needs::Needs;
use crate::simulation::person::Profession;
use crate::simulation::skills::Skills;

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
}

/// Resource: the live list of scorers. Built once at
/// `SimulationPlugin::build`; queried per-agent per-tick.
#[derive(Resource, Default)]
pub struct GoalScorerRegistry {
    pub scorers: Vec<Box<dyn GoalScorer>>,
}

impl GoalScorerRegistry {
    /// Argmax over every scorer for one agent. Returns `None` when
    /// every scorer declines (e.g. all gates failed). Ties broken by
    /// `class` first (higher wins), then `score` within a class.
    pub fn best(&self, ctx: &GoalScoringContext) -> Option<GoalScore> {
        let mut best: Option<GoalScore> = None;
        for scorer in &self.scorers {
            let Some(candidate) = scorer.score(ctx) else {
                continue;
            };
            let take = match best {
                None => true,
                Some(cur) => match candidate.class.cmp(&cur.class) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => candidate.score > cur.score,
                },
            };
            if take {
                best = Some(candidate);
            }
        }
        best
    }
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
        let (kind, score) = best?;
        Some(GoalScore {
            goal: kind.to_goal(),
            class: GoalClass::Enterprise,
            score,
            reason: "Earning Income",
        })
    }

    fn name(&self) -> &'static str {
        "EarnIncomeScorer"
    }
}

/// Convenience: install the default scorer set on a `GoalScorerRegistry`.
/// Today: just `EarnIncomeScorer`. Future plans add `SocializeScorer`
/// (Belonging), `EsteemScorer` (Esteem — paying for prestige craft
/// commissions), `HealSeekerScorer` (Survival — injured agent walks to
/// a Healer), etc. Each new scorer is one push.
pub fn register_default_scorers(registry: &mut GoalScorerRegistry) {
    registry.scorers.push(Box::new(EarnIncomeScorer));
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
    fn registry_argmax_breaks_ties_by_class_first() {
        let mut registry = GoalScorerRegistry::default();
        struct StubScorer {
            goal: AgentGoal,
            class: GoalClass,
            score: f32,
        }
        impl GoalScorer for StubScorer {
            fn score(&self, _ctx: &GoalScoringContext) -> Option<GoalScore> {
                Some(GoalScore {
                    goal: self.goal,
                    class: self.class,
                    score: self.score,
                    reason: "stub",
                })
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
        };
        let best = registry.best(&ctx).expect("at least one stub fires");
        assert_eq!(best.goal, AgentGoal::Sleep);
        assert_eq!(best.class, GoalClass::Subsistence);
    }
}
