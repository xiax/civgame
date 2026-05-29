//! Per-person technology knowledge.
//!
//! Each Person carries a `PersonKnowledge` component holding two bitsets:
//! `aware` (heard of it, no capacity cost, gossiped freely) and `learned`
//! (can perform/teach, subset of `aware`). There is no hard cap on Learned
//! techs; instead, learning *speed* slows as the agent's stack of Learned
//! complexity grows: `slowdown = 1 + complexity_used / (intelligence × 2)`.
//! Applied to study (Read/Lecture/Teach via `add_study_progress`) and to the
//! passive teaching per-tick roll. Per-action discovery is unaffected.
//!
//! Discovery is driven per-action by `try_discover_from_action`, called from
//! the existing yield/combat hooks (gather, production, combat, plants).
//! The faction's collective `FactionTechs` is a derived projection of the
//! chief's `aware` bitset (see `faction::sync_faction_techs_from_chief_system`).
use bevy::prelude::*;

use super::knowledge_bits::KnowledgeBits;
use super::skills::{SkillKind, Skills};
use super::stats::{modifier, Stats};
use super::technology::{
    complexity, tech_def, ActivityKind, TechId, TechTrigger, TECH_COUNT, TECH_TREE,
};

/// Legacy capacity constant for the per-tech `learned_at` array. Retained as a
/// public alias because external test fixtures and reproduction sites named it
/// at construction time; the field itself is now an `AHashMap`, so this value
/// only feeds initial-capacity hints today.
pub const KNOWLEDGE_SLOTS: usize = 64;

/// One game-day of study per complexity point. Cuneiform (complexity 6) takes
/// 6 days of solo reading; a paleolithic tech takes one day. Lectures and
/// 1-on-1 teaching contribute at higher rates.
pub const STUDY_TICKS_PER_COMPLEXITY: u32 = crate::world::seasons::TICKS_PER_DAY;

/// Per-person knowledge state.
///
/// `aware`: bit set if the person has *heard of* a tech. Free, gossipable.
/// `learned`: bit set if the person has mastered the tech. Costs complexity
/// points (see `complexity()`); always a subset of `aware`.
/// What role a founder fills at game-start. Drives
/// [`PersonKnowledge::seeded_realistic_through_era`] — chiefs / scribes
/// learn institutional techs, specialists learn the workshop techs, and
/// the rest of the band has band-wide common knowledge only (everyone
/// remains Aware of the full era, so awareness gossip + study can still
/// spread mastery through normal play).
#[derive(Clone, Copy, Debug)]
pub enum FounderRole {
    /// Default founder. Personal / Household / Subsistence techs Learned;
    /// Specialist / Institutional techs Aware only.
    Common,
    /// One per `~members/8`. Adds Specialist + MilitaryTransport techs
    /// to the Learned set.
    Specialist,
    /// The first member spawned per faction. Adds Specialist +
    /// Institutional techs on top of the common pool.
    Chief,
    /// Reserved for future use (a literate scribe inside an Institutional
    /// faction). Equivalent to `Chief` for Institutional gating today.
    Scribe,
}

/// Maximum mastery level. `mastery_speed_mult` rises monotonically through this
/// range; mastery > `MASTERY_MAX` saturates at the cap.
pub const MASTERY_MAX: u8 = 3;

/// Maximum count of previously-rejected ids tracked per belief group.
/// Three is plenty for cosmology / disease_causation through the ancient
/// core (Sky Dome → Geocentric → Heliocentric is the longest chain).
pub const BELIEF_REJECTED_CAP: usize = 3;

/// Belief held by one agent in one belief group. Confidence is `0..=255`;
/// `rejected` carries up to three formerly-accepted ids so future content can
/// distinguish "never heard of" from "considered and dismissed". `rejected_len`
/// tracks the live prefix of `rejected`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BeliefState {
    /// Currently-accepted knowledge id in this group.
    pub accepted: TechId,
    /// 0..=255. Threshold for swap-on-study lands content-side in Phase H.
    pub confidence: u8,
    /// Stack of previously-accepted ids the agent now rejects.
    pub rejected: [TechId; BELIEF_REJECTED_CAP],
    /// Live prefix length of `rejected` (`0..=BELIEF_REJECTED_CAP`).
    pub rejected_len: u8,
}

impl BeliefState {
    /// Push `id` onto the rejected stack, dropping the oldest entry when
    /// the cap is full (FIFO eviction so the most-recent rejection always
    /// stays). Safe to call with a duplicate id — it bubbles to the end.
    pub fn push_rejected(&mut self, id: TechId) {
        // De-dupe: if `id` already present, drop it first.
        let mut write = 0usize;
        for read in 0..self.rejected_len as usize {
            if self.rejected[read] != id {
                self.rejected[write] = self.rejected[read];
                write += 1;
            }
        }
        // Drop the oldest if at cap.
        if write >= BELIEF_REJECTED_CAP {
            for i in 0..BELIEF_REJECTED_CAP - 1 {
                self.rejected[i] = self.rejected[i + 1];
            }
            write = BELIEF_REJECTED_CAP - 1;
        }
        self.rejected[write] = id;
        self.rejected_len = (write as u8 + 1).min(BELIEF_REJECTED_CAP as u8);
    }

    /// Iterate the live rejected ids in insertion order.
    pub fn rejected_iter(&self) -> impl Iterator<Item = TechId> + '_ {
        self.rejected[..self.rejected_len as usize].iter().copied()
    }
}

/// `learned_at`: last tick each tech was learned or refreshed (used or taught).
/// Sparse map keyed by id so non-Learned ids carry no storage; replaces the
/// legacy fixed `[u32; 64]` array now that the catalog can grow past 64
/// entries.
///
/// `mastery` and `belief` are Phase-C additions, both sparse + empty by
/// default — every existing gate site reads them as 0 / absent, so behaviour
/// is unchanged until later content phases write values in.
#[derive(Component, Clone, Debug, Default)]
pub struct PersonKnowledge {
    pub aware: KnowledgeBits,
    pub learned: KnowledgeBits,
    pub learned_at: ahash::AHashMap<TechId, u32>,
    /// Sparse map TechId → progress ticks accumulated toward Learned. Cleared
    /// on successful learn or eviction. Used by Phase-2 reading/lecture/teach
    /// systems; the original passive `tech_teaching_system` and
    /// `try_discover_from_action` paths bypass it (they roll directly).
    pub study_progress: ahash::AHashMap<TechId, u32>,
    /// Per-skill mastery level (0..=`MASTERY_MAX`). Sparse — absent ids read
    /// as 0. Only meaningful for `KnowledgeKind::PracticalSkill` /
    /// `PracticalTechnique` entries; reading mastery for a `Belief` is
    /// undefined (always 0).
    pub mastery: ahash::AHashMap<TechId, u8>,
    /// Per-group belief state. Sparse — absent groups read as "no belief held
    /// in this group". `KnowledgeKind::Belief` entries are the only ones that
    /// populate this.
    pub belief: ahash::AHashMap<
        crate::simulation::knowledge_catalog::BeliefGroupId,
        BeliefState,
    >,
}

impl PersonKnowledge {
    /// Seed a brand-new agent with all Paleolithic techs both Aware and Learned.
    pub fn paleolithic_seed(now: u32) -> Self {
        Self::seeded_through_era(super::technology::Era::Paleolithic, now)
    }

    /// Seed a brand-new agent with every tech whose era is at or below
    /// `target` set both Aware and Learned. Retained for tests and
    /// fixtures that need an "everyone knows everything" baseline.
    /// Production gameplay should use `seeded_realistic_through_era`,
    /// which scopes Specialist + Institutional techs to a fraction of
    /// founders so a band doesn't spawn with universal blacksmiths.
    pub fn seeded_through_era(target: super::technology::Era, now: u32) -> Self {
        let mut k = Self::default();
        let target_rank = target as u8;
        for def in TECH_TREE.iter() {
            if (def.era as u8) <= target_rank {
                k.aware.set(def.id);
                k.learned.set(def.id);
                k.learned_at.insert(def.id, now);
            }
        }
        k
    }

    /// Realistic founder seeding scoped by `role`:
    /// - `Personal` / `Household` / `Subsistence` techs land Aware+Learned
    ///   on everyone (the band-wide common knowledge).
    /// - `Specialist` techs land Aware on everyone, but Learned only on
    ///   `FounderRole::Specialist` or `FounderRole::Chief`.
    /// - `Institutional` techs land Aware on everyone, Learned only on
    ///   `FounderRole::Chief` (or `FounderRole::Scribe`).
    ///
    /// Phase 1's `derive_tech_adoption_system` picks up the resulting
    /// community state on the next tick: common techs hit `Adopted` via
    /// broad-learning; specialist techs hit `Adopted` via the
    /// small-band-with-practitioner shortcut; institutional techs hit
    /// `Adopted` via the chief-learned + prereqs path.
    pub fn seeded_realistic_through_era(
        target: super::technology::Era,
        role: FounderRole,
        now: u32,
    ) -> Self {
        use super::knowledge_catalog::{knowledge_def, KnowledgeKind};
        use super::technology_adoption::{tech_scale, AdoptionScale};
        let mut k = Self::default();
        let target_rank = target as u8;
        for def in TECH_TREE.iter() {
            if (def.era as u8) > target_rank {
                continue;
            }
            k.aware.set(def.id);
            // Phase H — `KnowledgeKind::Belief` entries are held, not
            // Learned. `seed_initial_beliefs` populates the per-group
            // belief map; the `learned` bitset stays off so nothing
            // downstream (technique selection, recipe gating, mastery
            // accrual) treats a belief as a working skill.
            if matches!(knowledge_def(def.id).kind(), KnowledgeKind::Belief) {
                continue;
            }
            let should_learn = match (tech_scale(def.id), role) {
                (
                    AdoptionScale::Personal | AdoptionScale::Household | AdoptionScale::Subsistence,
                    _,
                ) => true,
                (
                    AdoptionScale::Specialist | AdoptionScale::MilitaryTransport,
                    FounderRole::Chief | FounderRole::Specialist | FounderRole::Scribe,
                ) => true,
                (AdoptionScale::Institutional, FounderRole::Chief | FounderRole::Scribe) => true,
                _ => false,
            };
            if should_learn {
                k.learned.set(def.id);
                k.learned_at.insert(def.id, now);
            }
        }
        // Phase H — seed initial belief acceptance per era. Cosmology,
        // disease_causation, and omens each pin to an era-appropriate
        // accepted id at high confidence; gameplay can shift these via
        // study (Phase H.2+ belief-swap logic) over time.
        seed_initial_beliefs(&mut k, target);
        k
    }

    #[inline]
    pub fn is_aware(&self, id: TechId) -> bool {
        self.aware.has(id)
    }

    #[inline]
    pub fn has_learned(&self, id: TechId) -> bool {
        self.learned.has(id)
    }

    /// Snapshot of this person's awareness ∪ Learned as a `KnowledgeBits`
    /// bag. Used by reproduction inheritance and awareness-gossip merges.
    #[inline]
    pub fn awareness_snapshot(&self) -> KnowledgeBits {
        self.aware.union(&self.learned)
    }

    /// Snapshot of this person's Learned set as a `FactionTechs` bitset.
    /// Used by the construction poster pool: a poster's snapshot becomes
    /// the blueprint's `design_techs`, locking tier picks at intent time
    /// so a build started under one chief survives succession.
    #[inline]
    pub fn learned_bitset(&self) -> super::faction::FactionTechs {
        super::faction::FactionTechs(self.learned)
    }

    /// OR another agent's awareness into ours (gossip transfer).
    pub fn merge_awareness(&mut self, other_aware: &KnowledgeBits) {
        self.aware.union_assign(other_aware);
    }

    /// Sum of complexity points across currently-Learned techs.
    pub fn complexity_used(&self) -> u16 {
        let mut total: u16 = 0;
        for id in 0..TECH_COUNT as TechId {
            if self.has_learned(id) {
                total = total.saturating_add(complexity(id) as u16);
            }
        }
        total
    }

    /// Add `id` to Learned. No-op if already Learned (still refreshes
    /// `learned_at`). There is no hard cap; learning *speed* slows with
    /// stack size — see `learning_slowdown`.
    pub fn try_learn(&mut self, id: TechId, now: u32) -> LearnOutcome {
        if self.has_learned(id) {
            self.learned_at.insert(id, now);
            return LearnOutcome::AlreadyKnown;
        }
        self.aware.set(id);
        self.learned.set(id);
        self.learned_at.insert(id, now);
        LearnOutcome::Learned
    }
}

/// Outcome of a `try_learn` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnOutcome {
    /// Newly learned.
    Learned,
    /// Already in the Learned set; recency was refreshed.
    AlreadyKnown,
}

/// Outcome of a single `add_study_progress` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudyOutcome {
    /// Progress accumulated, threshold not yet met.
    InProgress { progress: u32, threshold: u32 },
    /// Threshold reached; tech is now Learned.
    Learned,
    /// Already in Learned set; nothing to do.
    AlreadyLearned,
}

/// Threshold of study ticks needed to learn `tech`. Scales with `complexity`
/// so paleolithic techs take ~1 game-day, Bronze Age ~5, Cuneiform ~6.
#[inline]
pub fn study_threshold(tech: TechId) -> u32 {
    complexity(tech) as u32 * STUDY_TICKS_PER_COMPLEXITY
}

impl PersonKnowledge {
    /// Add `amount` study points toward learning `tech`. Always grants
    /// awareness (mirrors "I cracked open the book" — you've now heard of it).
    /// `amount` is divided by `learning_slowdown(stats, self)` before
    /// accumulating, so heavy stacks make slower per-tick progress.
    /// On reaching `study_threshold(tech)`, runs `try_learn` and clears
    /// progress. Returns the StudyOutcome so callers can log.
    pub fn add_study_progress(
        &mut self,
        tech: TechId,
        amount: u32,
        stats: &Stats,
        now: u32,
    ) -> StudyOutcome {
        if self.has_learned(tech) {
            self.learned_at.insert(tech, now);
            return StudyOutcome::AlreadyLearned;
        }
        // Awareness is free.
        self.aware.set(tech);
        let slowdown = learning_slowdown(stats, self);
        let scaled = ((amount as f32) / slowdown).round() as u32;
        // Floor at 1 so a single study tick still nudges progress at extreme
        // stacks; otherwise zero-amount ticks would stall heavy learners.
        let scaled = scaled.max(1);
        let entry = self.study_progress.entry(tech).or_insert(0);
        *entry = entry.saturating_add(scaled);
        let threshold = study_threshold(tech);
        if *entry >= threshold {
            self.study_progress.remove(&tech);
            return match self.try_learn(tech, now) {
                LearnOutcome::Learned => StudyOutcome::Learned,
                LearnOutcome::AlreadyKnown => StudyOutcome::AlreadyLearned,
            };
        }
        let progress = *entry;
        StudyOutcome::InProgress {
            progress,
            threshold,
        }
    }
}

/// Phase H — seed the belief map for a freshly-spawned founder at era
/// `target`. Cosmology / disease_causation / omens groups each pin to an
/// era-appropriate accepted `KnowledgeId` at high confidence.
///
/// - **Pre-Neolithic** (`Paleolithic` / `Mesolithic`) — Sky Dome cosmology,
///   Spirit Illness disease model. Both held with high confidence.
/// - **Neolithic+** — Geocentric cosmology, Miasma disease model. The
///   agricultural revolution introduced systematic sky-watching + sanitation
///   priorities; the FalseUseful members of each group drive better behaviour
///   than their pre-Neolithic counterparts even though both are wrong.
/// - **Every era** — Weather Omens accepted in the `omens` group (Eclipse
///   Omens reserved for the omens group's pre-Neolithic belief; later
///   content can swap one for the other on study).
fn seed_initial_beliefs(k: &mut PersonKnowledge, target: super::technology::Era) {
    use super::knowledge_catalog::{
        BELIEF_GROUP_COSMOLOGY, BELIEF_GROUP_DISEASE_CAUSATION, BELIEF_GROUP_OMENS,
    };
    use super::technology::{
        Era, ECLIPSE_OMENS, GEOCENTRIC_COSMOS, MIASMA_THEORY, SKY_DOME, SPIRIT_ILLNESS,
        WEATHER_OMENS,
    };
    const HIGH_CONFIDENCE: u8 = 200;
    const MEDIUM_CONFIDENCE: u8 = 140;
    let neolithic_or_later = (target as u8) >= Era::Neolithic as u8;
    // Cosmology
    let cosmo_accept = if neolithic_or_later {
        GEOCENTRIC_COSMOS
    } else {
        SKY_DOME
    };
    k.accept_belief(BELIEF_GROUP_COSMOLOGY, cosmo_accept, HIGH_CONFIDENCE);
    // Disease causation
    let disease_accept = if neolithic_or_later {
        MIASMA_THEORY
    } else {
        SPIRIT_ILLNESS
    };
    k.accept_belief(
        BELIEF_GROUP_DISEASE_CAUSATION,
        disease_accept,
        HIGH_CONFIDENCE,
    );
    // Omens — Pre-Neolithic agents lean Eclipse-fearful; Neolithic+ agents
    // hold Weather Omens at the foreground (eclipse fear lingers as a
    // rejected memory once the band reaches systematic calendar-keeping).
    let omens_accept = if neolithic_or_later {
        WEATHER_OMENS
    } else {
        ECLIPSE_OMENS
    };
    k.accept_belief(BELIEF_GROUP_OMENS, omens_accept, MEDIUM_CONFIDENCE);
}

/// Multiplier applied to learning *time*. 1.0 = full speed (empty stack);
/// 2.0 at the old `intelligence × 2` baseline; 3.0 at twice that. Smooth in
/// both `complexity_used` and `intelligence` — no threshold cliffs.
#[inline]
pub fn learning_slowdown(stats: &Stats, k: &PersonKnowledge) -> f32 {
    let baseline = (stats.intelligence as f32) * 2.0;
    1.0 + (k.complexity_used() as f32) / baseline.max(1.0)
}

// ── Mastery + belief helpers ─────────────────────────────────────────────────

impl PersonKnowledge {
    /// Per-skill mastery level (0..=`MASTERY_MAX`). Absent ids read as 0.
    #[inline]
    pub fn mastery_of(&self, id: TechId) -> u8 {
        self.mastery.get(&id).copied().unwrap_or(0)
    }

    /// Raise mastery in `id` by `delta`, saturating at `MASTERY_MAX`. Returns
    /// the new level. No-op for `delta == 0`.
    pub fn gain_mastery(&mut self, id: TechId, delta: u8) -> u8 {
        if delta == 0 {
            return self.mastery_of(id);
        }
        let entry = self.mastery.entry(id).or_insert(0);
        *entry = entry.saturating_add(delta).min(MASTERY_MAX);
        *entry
    }

    /// Currently-accepted belief in `group`, if any.
    #[inline]
    pub fn belief_in(
        &self,
        group: crate::simulation::knowledge_catalog::BeliefGroupId,
    ) -> Option<&BeliefState> {
        self.belief.get(&group)
    }

    /// Accept `id` in `group` with `confidence`. If a different id was held,
    /// it's pushed onto the rejected stack. Idempotent for same-id calls (only
    /// confidence updates).
    pub fn accept_belief(
        &mut self,
        group: crate::simulation::knowledge_catalog::BeliefGroupId,
        id: TechId,
        confidence: u8,
    ) {
        let state = self.belief.entry(group).or_insert(BeliefState::default());
        if state.accepted != id && (state.confidence > 0 || state.rejected_len > 0) {
            // Demote the prior accepted id onto the rejected stack.
            state.push_rejected(state.accepted);
        }
        state.accepted = id;
        state.confidence = confidence;
    }

    /// Reject `id` outright in `group` without electing a replacement. Pushes
    /// onto the rejected stack and zeroes confidence. If `id == accepted` the
    /// accepted slot is cleared (defaulting to id 0); callers typically pair
    /// this with a subsequent `accept_belief`.
    pub fn reject_belief(
        &mut self,
        group: crate::simulation::knowledge_catalog::BeliefGroupId,
        id: TechId,
    ) {
        let state = self.belief.entry(group).or_insert(BeliefState::default());
        state.push_rejected(id);
        if state.accepted == id {
            state.accepted = 0;
            state.confidence = 0;
        }
    }
}

/// Multiplier on per-tick work progress for an agent with the given mastery
/// level in the relevant skill. Level 0 (no mastery) → 1.0× (unchanged).
/// Above zero: 1.10 / 1.20 / 1.35 for levels 1 / 2 / 3. Default behaviour at
/// game start is unchanged because every agent's `mastery` map is empty.
#[inline]
pub fn mastery_speed_mult(level: u8) -> f32 {
    match level.min(MASTERY_MAX) {
        0 => 1.0,
        1 => 1.10,
        2 => 1.20,
        _ => 1.35,
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// Map ActivityKind to the SkillKind whose XP scales discovery probability.
pub fn activity_to_skill(activity: ActivityKind) -> SkillKind {
    match activity {
        ActivityKind::Foraging | ActivityKind::Farming => SkillKind::Farming,
        ActivityKind::WoodGathering => SkillKind::Building,
        ActivityKind::StoneMining
        | ActivityKind::CoalMining
        | ActivityKind::IronMining
        | ActivityKind::CopperMining
        | ActivityKind::TinMining
        | ActivityKind::GoldMining
        | ActivityKind::SilverMining => SkillKind::Mining,
        ActivityKind::Combat => SkillKind::Combat,
        ActivityKind::Socializing => SkillKind::Social,
        ActivityKind::Trading => SkillKind::Trading,
        ActivityKind::Fishing => SkillKind::Fishing,
    }
}

/// Per-action discovery roll. For each tech whose triggers include `activity`
/// and whose prerequisites the person has personally Learned, roll
/// `base * (1 + int_mod) * (1 + skill_xp / 1000)`. On success, set Aware and
/// jump-start `study_progress` by `complexity * INSIGHT_PROGRESS_PER_COMPLEXITY`
/// (capped strictly below the learn threshold). Insight is one-shot per agent
/// per tech — once Aware, further rolls on the same tech are skipped so
/// repeated foraging doesn't re-fire the event.
pub fn try_discover_from_action(
    knowledge: &mut PersonKnowledge,
    stats: &Stats,
    skills: &Skills,
    activity: ActivityKind,
    now: u32,
) -> Option<TechId> {
    let _ = now;
    let int_mod = modifier(stats.intelligence) as f32;
    let int_scale = 1.0 + (int_mod * 0.1).max(-0.4);
    let skill = activity_to_skill(activity);
    let skill_xp = skills.get(skill) as f32;
    let skill_scale = 1.0 + (skill_xp / 1000.0).min(2.0);

    for def in TECH_TREE.iter() {
        if knowledge.is_aware(def.id) {
            continue;
        }
        // Prerequisites: must have personally Learned every prereq (the
        // "next-level adjacent" rule).
        if !def.prerequisites.iter().all(|&p| knowledge.has_learned(p)) {
            continue;
        }
        let trigger_chance: f32 = def
            .triggers
            .iter()
            .filter(|t: &&TechTrigger| t.activity == activity)
            .map(|t| t.per_unit_chance)
            .sum();
        if trigger_chance <= 0.0 {
            continue;
        }
        let chance = (trigger_chance * int_scale * skill_scale).min(0.5);
        if fastrand::f32() < chance {
            knowledge.aware.set(def.id);
            let threshold = study_threshold(def.id);
            let bump = complexity(def.id) as u32 * INSIGHT_PROGRESS_PER_COMPLEXITY;
            let capped = bump.min(threshold.saturating_sub(1));
            let entry = knowledge.study_progress.entry(def.id).or_insert(0);
            *entry = (*entry)
                .saturating_add(capped)
                .min(threshold.saturating_sub(1));
            return Some(def.id);
        }
    }
    None
}

/// Study-progress jump-start granted by a single discovery insight, per unit
/// of tech complexity. `complexity × this` is capped strictly below
/// `study_threshold(tech) = complexity × STUDY_TICKS_PER_COMPLEXITY`,
/// so insight never auto-completes — the agent still needs Read/Lecture/Teach
/// to push over the threshold.
pub const INSIGHT_PROGRESS_PER_COMPLEXITY: u32 = 1200;

/// Helper: capacity for a person from their stats.
#[inline]
pub fn capacity_for(stats: &Stats) -> u16 {
    super::stats::knowledge_capacity(stats.intelligence)
}

/// Emitted by every site that performs a knowledge-relevant action (gather,
/// farm, combat, social, mining, etc.). Consumed by `discovery_system` to roll
/// per-action tech discovery against the actor's PersonKnowledge.
#[derive(Event, Clone, Copy, Debug)]
pub struct DiscoveryActionEvent {
    pub actor: Entity,
    pub activity: ActivityKind,
}

/// Round-robin cursor over social-contact agents for the per-tick
/// awareness + wage gossip merge passes. Both systems advance this cursor
/// in lock-step so the same slice runs through both gossip channels on a
/// given tick — one snapshot, two consumers.
#[derive(Resource, Default)]
pub struct GossipCursor {
    pub next_entity_bits: u64,
}

/// Tech-awareness gossip between agents within 3 tiles whose goal is
/// `Socialize`. Awareness is free (single bit) and propagates only between
/// socializing agents; mastery (Learned) only spreads via the explicit
/// `tech_teaching_system` chance roll.
///
/// Phase 3.2: per-tick cursor amortisation. The snapshot pass stays full
/// (cheap copy of `KnowledgeBits` + small `Vec<SettlementId>`); the merge
/// pass — the expensive O(N × 49) part — only runs for a slice of
/// `budget.gossip_agents_per_tick` agents per tick. Each agent is revisited
/// every ~`N_social / cap` ticks, well below the awareness-bit propagation
/// horizon.
///
/// Lifted from the deleted `plan_gossip_system` which also gossiped
/// `KnownPlans` entries — that half is gone with the plan/ module retirement
/// in Phase 7. Runs in `SimulationSet::Economy` after
/// `conversation_memory_system`, before `tech_teaching_system`.
pub fn awareness_gossip_system(
    budget: Res<crate::simulation::perf::PerfWorkBudget>,
    mut cursor: ResMut<GossipCursor>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<crate::simulation::schedule::SimClock>,
    mut q: Query<(
        Entity,
        &Transform,
        &super::goals::AgentGoal,
        &super::lod::LodLevel,
        &mut PersonKnowledge,
        Option<&mut super::memory::AgentMemory>,
        Option<&crate::simulation::social_contact::SecondarySocial>,
    )>,
    timings: Res<crate::simulation::speed::SuspectSystemTimings>,
) {
    use crate::simulation::social_contact::is_social_contact;
    let _t = timings.guard(crate::simulation::speed::suspect::AWARENESS_GOSSIP);
    let now = clock.tick as u32;

    // Snapshot each socializing agent's tech-awareness (aware|learned)
    // AND visited settlements (Pluralist Economy R8 follow-on). Both
    // OR-merge between adjacent socializing agents; gossip is free,
    // teaching (Learned) is the bottleneck via tech_teaching_system.
    let snapshots: ahash::AHashMap<
        Entity,
        (
            KnowledgeBits,
            Vec<crate::simulation::settlement::SettlementId>,
        ),
    > = q
        .iter()
        .filter(|(_, _, goal, lod, _, _, sec)| is_social_contact(**goal, **lod, *sec, now))
        .map(|(e, _, _, _, k, mem_opt, _)| {
            let aware = k.awareness_snapshot();
            let settlements: Vec<_> = mem_opt
                .as_deref()
                .map(|m| m.known_settlements().map(|(id, _)| id).collect())
                .unwrap_or_default();
            (e, (aware, settlements))
        })
        .collect();

    if snapshots.is_empty() {
        return;
    }

    // Build a sorted list of socializing-agent entities for the cursor.
    let mut social_entities: Vec<Entity> = snapshots.keys().copied().collect();
    social_entities.sort_unstable_by_key(|e| e.to_bits());

    let cap = budget.gossip_agents_per_tick.max(1).min(social_entities.len());
    let pivot = social_entities
        .iter()
        .position(|e| e.to_bits() >= cursor.next_entity_bits)
        .unwrap_or(0);
    let slice: ahash::AHashSet<Entity> = (0..cap)
        .map(|offset| social_entities[(pivot + offset) % social_entities.len()])
        .collect();
    // Advance cursor past the last entity in the slice.
    if let Some(&last) = (0..cap)
        .map(|offset| &social_entities[(pivot + offset) % social_entities.len()])
        .last()
    {
        cursor.next_entity_bits = last.to_bits().saturating_add(1);
    }

    for (entity, transform, goal, lod, mut knowledge, mem_opt, sec) in q.iter_mut() {
        if !is_social_contact(*goal, *lod, sec, now) {
            continue;
        }
        if !slice.contains(&entity) {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE_LOCAL).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE_LOCAL).floor() as i32;
        let mut aware_received = KnowledgeBits::EMPTY;
        let mut settlements_received: ahash::AHashSet<crate::simulation::settlement::SettlementId> =
            ahash::AHashSet::default();
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if let Some((snap_aware, snap_settlements)) = snapshots.get(&other) {
                        aware_received.union_assign(snap_aware);
                        for sid in snap_settlements {
                            settlements_received.insert(*sid);
                        }
                    }
                }
            }
        }
        if !aware_received.is_empty() {
            knowledge.merge_awareness(&aware_received);
        }
        if !settlements_received.is_empty() {
            if let Some(mut memory) = mem_opt {
                for sid in settlements_received {
                    memory.record_settlement(sid);
                }
            }
        }
    }
}

/// Promote `SharedKnowledge` clusters up the tier ladder when constituents
/// gossip with officials. Phase 6 of the memory overhaul.
///
/// **Household → Settlement.** When a `HouseholdMember` socialises within 3
/// tiles of a `Profession::Bureaucrat` of the same root faction, every
/// cluster in `KnowledgeTier::Household(hid)` is reported into
/// `KnowledgeTier::Settlement(sid)` (idempotent — `report_sighting` merges
/// nearby same-(kind, owner) clusters, so repeated bridge events don't
/// inflate the settlement map). Models the bureaucrat as the conduit: a
/// settlement only "knows" what its members have told an official.
///
/// **Settlement → Faction.** When two same-faction officials (Bureaucrats or
/// `FactionChief`) socialise within 3 tiles, the lower's settlement-tier
/// clusters bubble up to `KnowledgeTier::Faction(fid)`. Lets traders /
/// cross-settlement chiefs see arbitrage geography even from settlements
/// they've never visited.
///
/// Runs every `TIER_PROMOTION_CADENCE` ticks to bound cost. Reads positions,
/// professions, household / faction membership; mutates `SharedKnowledge`.
pub fn cluster_tier_promotion_system(
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<crate::simulation::schedule::SimClock>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    faction_registry: Res<crate::simulation::faction::FactionRegistry>,
    mut shared: ResMut<crate::simulation::shared_knowledge::SharedKnowledge>,
    q: Query<(
        Entity,
        &Transform,
        &super::goals::AgentGoal,
        &super::lod::LodLevel,
        &super::person::Profession,
        Option<&super::faction::FactionMember>,
        Option<&super::faction::FactionChief>,
        Option<&crate::simulation::reproduction::HouseholdMember>,
        Option<&crate::simulation::social_contact::SecondarySocial>,
    )>,
) {
    use super::person::Profession;
    use crate::simulation::shared_knowledge::KnowledgeTier;
    use crate::simulation::social_contact::is_social_contact;

    const TIER_PROMOTION_CADENCE: u64 = 200; // 10s game-time
    if clock.tick % TIER_PROMOTION_CADENCE != 0 {
        return;
    }
    let now_contact = clock.tick as u32;

    // Snapshot socializing agents with their faction / household / official
    // status, keyed by tile for cheap neighbour lookup.
    struct Snap {
        entity: Entity,
        tile: (i32, i32),
        faction_id: u32,
        household_id: Option<u32>,
        is_bureaucrat: bool,
        is_chief: bool,
    }
    let snaps: Vec<Snap> = q
        .iter()
        .filter(|(_, _, goal, lod, _, _, _, _, sec)| {
            is_social_contact(**goal, **lod, *sec, now_contact)
        })
        .filter_map(|(e, t, _, _, profession, fm, chief, hm, _)| {
            let fm = fm?;
            let tx = (t.translation.x / TILE_SIZE_LOCAL).floor() as i32;
            let ty = (t.translation.y / TILE_SIZE_LOCAL).floor() as i32;
            Some(Snap {
                entity: e,
                tile: (tx, ty),
                faction_id: fm.faction_id,
                household_id: hm.map(|h| h.household_id),
                is_bureaucrat: matches!(profession, Profession::Bureaucrat),
                is_chief: chief.is_some(),
            })
        })
        .collect();
    if snaps.is_empty() {
        return;
    }

    // Pass 1: identify (household_id, settlement_id) pairs to promote.
    // The bureaucrat's faction's *first settlement* is the target tier
    // (mirrors `SettlementMap::first_for_faction` semantics elsewhere in
    // the codebase). A household's root faction (walking parent chain) is
    // what we match against the bureaucrat's faction.
    let mut household_to_settlement: ahash::AHashSet<(
        u32,
        crate::simulation::settlement::SettlementId,
    )> = ahash::AHashSet::default();
    let mut settlement_to_faction: ahash::AHashSet<(
        crate::simulation::settlement::SettlementId,
        u32,
    )> = ahash::AHashSet::default();
    for s in &snaps {
        if !(s.is_bureaucrat || s.is_chief) {
            continue;
        }
        // This snap is an official. Find their settlement.
        let Some(official_settlement) = settlement_map.first_for_faction(s.faction_id) else {
            continue;
        };
        // Scan neighbours within 3 tiles for partners.
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(s.tile.0 + dx, s.tile.1 + dy) {
                    if other == s.entity {
                        continue;
                    }
                    let Some(o) = snaps.iter().find(|x| x.entity == other) else {
                        continue;
                    };
                    // Same-faction-root check: the other agent's faction
                    // must walk back to the official's faction (covers the
                    // common case where a household member's
                    // FactionMember.faction_id is still the village id —
                    // confirmed by the existing R3 doc).
                    if faction_registry.root_faction(o.faction_id) != s.faction_id {
                        continue;
                    }
                    // Household → Settlement: o is a household member, s
                    // is an official in the household's faction.
                    if let Some(hid) = o.household_id {
                        household_to_settlement.insert((hid, official_settlement));
                    }
                    // Settlement → Faction: both are officials of the same
                    // faction, possibly at different settlements.
                    if o.is_bureaucrat || o.is_chief {
                        if let Some(o_settlement) = settlement_map.first_for_faction(o.faction_id) {
                            settlement_to_faction.insert((o_settlement, s.faction_id));
                        }
                    }
                }
            }
        }
    }

    // Pass 2: actually copy clusters between tiers. `report_sighting` is
    // idempotent — same-(kind, owner) clusters within MERGE_RADIUS just
    // refresh `last_seen_tick`. Re-promotion is therefore cheap.
    let now = clock.tick;
    for (hid, sid) in household_to_settlement {
        let src = KnowledgeTier::Household(hid);
        let dst = KnowledgeTier::Settlement(sid);
        if let Some(map) = shared.tiers.get(&src) {
            // Snapshot to avoid borrow conflict during the copy loop.
            let entries: Vec<(
                (i32, i32),
                crate::simulation::memory::MemoryKind,
                crate::simulation::shared_knowledge::ResourceOwner,
            )> = map
                .clusters
                .values()
                .filter_map(|c| {
                    let rep = c
                        .representative_tiles
                        .iter()
                        .find_map(|s| *s)
                        .unwrap_or(c.center);
                    Some((rep, c.kind, c.owner))
                })
                .collect();
            for (tile, kind, owner) in entries {
                shared.report_sighting(dst, tile, kind, owner, now);
            }
        }
    }
    for (sid, fid) in settlement_to_faction {
        let src = KnowledgeTier::Settlement(sid);
        let dst = KnowledgeTier::Faction(fid);
        if let Some(map) = shared.tiers.get(&src) {
            let entries: Vec<(
                (i32, i32),
                crate::simulation::memory::MemoryKind,
                crate::simulation::shared_knowledge::ResourceOwner,
            )> = map
                .clusters
                .values()
                .filter_map(|c| {
                    let rep = c
                        .representative_tiles
                        .iter()
                        .find_map(|s| *s)
                        .unwrap_or(c.center);
                    Some((rep, c.kind, c.owner))
                })
                .collect();
            for (tile, kind, owner) in entries {
                shared.report_sighting(dst, tile, kind, owner, now);
            }
        }
    }
}

/// Teaching: if two socializing agents are within 3 tiles and one has Learned
/// a tech the other is aware-of-but-not-Learned, roll a small per-tick chance
/// to transfer mastery. Mirrors `plan_gossip_system`'s spatial scan; the teach
/// rate is much lower than awareness gossip because Learned is the bottleneck.
///
/// Only fires when both partners' goal is Socialize so casual proximity
/// doesn't accidentally turn into a lesson. Picks the highest-complexity
/// teachable tech (most "valuable" knowledge transferred first).
pub fn tech_teaching_system(
    spatial: Res<crate::world::spatial::SpatialIndex>,
    clock: Res<crate::simulation::schedule::SimClock>,
    player: Res<crate::simulation::faction::PlayerFaction>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    name_query: Query<&Name>,
    mut q: Query<(
        Entity,
        &Transform,
        &super::goals::AgentGoal,
        &Stats,
        &super::lod::LodLevel,
        &mut PersonKnowledge,
        Option<&super::faction::FactionMember>,
    )>,
) {
    use super::goals::AgentGoal;
    use super::lod::LodLevel;

    // Pass 1: snapshot Learned bitsets from socializing agents.
    //
    // INTENTIONALLY not ambient-aware: deliberate mastery transfer stays
    // gated to explicit `AgentGoal::Socialize`. Do NOT consult
    // `social_contact::SecondarySocial` / `is_social_contact` here — casual
    // work chatter must not become accidental instruction (awareness/wage
    // gossip is free and does go ambient; *teaching* does not).
    let snapshots: ahash::AHashMap<Entity, KnowledgeBits> = q
        .iter()
        .filter(|(_, _, goal, _, lod, _, _)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .map(|(e, _, _, _, _, k, _)| (e, k.learned))
        .collect();

    if snapshots.len() < 2 {
        return;
    }

    let now = clock.tick as u32;

    // Pass 2: for each socializing student, find a teacher within 3 tiles
    // who has at least one teachable tech, then roll.
    for (entity, transform, goal, stats, lod, mut knowledge, fm) in q.iter_mut() {
        if *lod == LodLevel::Dormant || !matches!(goal, AgentGoal::Socialize) {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE_LOCAL).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE_LOCAL).floor() as i32;

        // Look for the nearby teacher with the largest set of teachable techs.
        let mut best_teach_set = KnowledgeBits::EMPTY;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    let Some(other_learned) = snapshots.get(&other) else {
                        continue;
                    };
                    // Teachable: teacher has Learned, student is Aware but not
                    // Learned. (`aware` is the shared awareness; the student
                    // needs to have heard of the tech first.)
                    let teachable = other_learned
                        .intersect(&knowledge.aware)
                        .difference(&knowledge.learned);
                    if teachable.count() > best_teach_set.count() {
                        best_teach_set = teachable;
                    }
                }
            }
        }

        if best_teach_set.is_empty() {
            continue;
        }

        // Per-tick teach chance: very small base rate, scaled by intelligence
        // modifier and divided by the student's learning slowdown so heavily
        // loaded students absorb new techs more slowly.
        let int_scale = 1.0 + (modifier(stats.intelligence) as f32 * 0.15).max(-0.5);
        let slowdown = learning_slowdown(stats, &knowledge);
        let chance = 0.004f32 * int_scale / slowdown;
        if fastrand::f32() >= chance {
            continue;
        }

        // Pick the highest-complexity teachable tech (most valuable lesson).
        let mut chosen: Option<TechId> = None;
        let mut chosen_cx: u8 = 0;
        for id in best_teach_set.iter() {
            let cx = complexity(id);
            if cx > chosen_cx {
                chosen = Some(id);
                chosen_cx = cx;
            }
        }
        let Some(tech_id) = chosen else { continue };

        if let LearnOutcome::Learned = knowledge.try_learn(tech_id, now) {
            // Player-facing notification mirrors discovery channel.
            if let Some(fm) = fm {
                if fm.faction_id == player.faction_id {
                    let def = tech_def(tech_id);
                    let _ = name_query.get(entity);
                    activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                        tick: clock.tick,
                        actor: entity,
                        faction_id: fm.faction_id,
                        kind: crate::ui::activity_log::ActivityEntryKind::TechDiscovered {
                            tech_name: def.name,
                            era_name: def.era.name(),
                        },
                    });
                }
            }
        }
    }
}

const TILE_SIZE_LOCAL: f32 = crate::world::terrain::TILE_SIZE;

/// Per-action discovery roller. Consumes `DiscoveryActionEvent`s emitted by
/// gather/production/combat/etc. and rolls per-actor against their
/// PersonKnowledge. On success, emits an ActivityLog tech-discovered entry for
/// the player faction.
pub fn discovery_system(
    mut events: EventReader<DiscoveryActionEvent>,
    clock: Res<crate::simulation::schedule::SimClock>,
    player: Res<crate::simulation::faction::PlayerFaction>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut q: Query<(
        Entity,
        &Stats,
        &Skills,
        &mut PersonKnowledge,
        Option<&crate::simulation::faction::FactionMember>,
    )>,
) {
    for ev in events.read() {
        let Ok((entity, stats, skills, mut knowledge, fm)) = q.get_mut(ev.actor) else {
            continue;
        };
        if let Some(tech_id) = try_discover_from_action(
            &mut knowledge,
            stats,
            skills,
            ev.activity,
            clock.tick as u32,
        ) {
            if let Some(fm) = fm {
                if fm.faction_id == player.faction_id {
                    let def = tech_def(tech_id);
                    activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                        tick: clock.tick,
                        actor: entity,
                        faction_id: fm.faction_id,
                        kind: crate::ui::activity_log::ActivityEntryKind::TechInsight {
                            tech_name: def.name,
                            era_name: def.era.name(),
                        },
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod mastery_belief_tests {
    use super::*;
    use crate::simulation::knowledge_catalog::BELIEF_GROUP_COSMOLOGY;

    #[test]
    fn mastery_accrual_saturates_at_max() {
        let mut k = PersonKnowledge::default();
        assert_eq!(k.mastery_of(7), 0);
        assert_eq!(k.gain_mastery(7, 1), 1);
        assert_eq!(k.gain_mastery(7, 1), 2);
        assert_eq!(k.gain_mastery(7, 5), MASTERY_MAX);
        assert_eq!(k.gain_mastery(7, 1), MASTERY_MAX);
        assert_eq!(k.mastery_of(7), MASTERY_MAX);
    }

    #[test]
    fn mastery_zero_delta_is_noop() {
        let mut k = PersonKnowledge::default();
        assert_eq!(k.gain_mastery(3, 0), 0);
        assert!(k.mastery.is_empty());
    }

    #[test]
    fn mastery_speed_mult_default_unchanged() {
        // The headline Phase-C invariant: an agent with no mastery reads as
        // exactly 1.0× work progress so existing tests / behaviour don't
        // shift until a future content phase writes mastery values.
        assert!((mastery_speed_mult(0) - 1.0).abs() < f32::EPSILON);
        assert!(mastery_speed_mult(1) > 1.0);
        assert!(mastery_speed_mult(MASTERY_MAX) > mastery_speed_mult(1));
        // Saturation at MASTERY_MAX.
        assert_eq!(
            mastery_speed_mult(MASTERY_MAX),
            mastery_speed_mult(MASTERY_MAX + 5)
        );
    }

    #[test]
    fn belief_accept_pushes_prior_onto_rejected() {
        let mut k = PersonKnowledge::default();
        // Start: nothing held in cosmology.
        assert!(k.belief_in(BELIEF_GROUP_COSMOLOGY).is_none());
        // Accept first model — no demotion yet.
        k.accept_belief(BELIEF_GROUP_COSMOLOGY, 10, 200);
        let s = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        assert_eq!(s.accepted, 10);
        assert_eq!(s.confidence, 200);
        assert_eq!(s.rejected_len, 0);
        // Swap to a different model — prior moves to rejected.
        k.accept_belief(BELIEF_GROUP_COSMOLOGY, 11, 180);
        let s = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        assert_eq!(s.accepted, 11);
        assert_eq!(s.rejected_len, 1);
        assert_eq!(s.rejected_iter().collect::<Vec<_>>(), vec![10]);
        // Swap again — second rejection stacks.
        k.accept_belief(BELIEF_GROUP_COSMOLOGY, 12, 220);
        let s = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        assert_eq!(s.rejected_iter().collect::<Vec<_>>(), vec![10, 11]);
    }

    #[test]
    fn belief_rejected_stack_bounded_to_cap() {
        let mut k = PersonKnowledge::default();
        // Swap through 5 ids; rejected slot only carries the last
        // `BELIEF_REJECTED_CAP` (FIFO eviction keeps recent rejections).
        for id in 10..15 {
            k.accept_belief(BELIEF_GROUP_COSMOLOGY, id, 100);
        }
        let s = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        assert_eq!(s.accepted, 14);
        assert_eq!(s.rejected_len as usize, BELIEF_REJECTED_CAP);
        // Oldest two (10, 11) dropped; last three (11, 12, 13) retained.
        // After 5 swaps the FIFO content is 11, 12, 13 (10 was dropped
        // first when 13 was rejected).
        assert_eq!(s.rejected_iter().collect::<Vec<_>>(), vec![11, 12, 13]);
    }

    #[test]
    fn belief_group_independence() {
        let mut k = PersonKnowledge::default();
        k.accept_belief(crate::simulation::knowledge_catalog::BELIEF_GROUP_COSMOLOGY, 10, 100);
        k.accept_belief(crate::simulation::knowledge_catalog::BELIEF_GROUP_DISEASE_CAUSATION, 20, 150);
        let cosmo = k
            .belief_in(crate::simulation::knowledge_catalog::BELIEF_GROUP_COSMOLOGY)
            .unwrap();
        let disease = k
            .belief_in(crate::simulation::knowledge_catalog::BELIEF_GROUP_DISEASE_CAUSATION)
            .unwrap();
        assert_eq!(cosmo.accepted, 10);
        assert_eq!(disease.accepted, 20);
    }
}
