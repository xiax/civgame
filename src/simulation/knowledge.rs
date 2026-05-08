//! Per-person technology knowledge.
//!
//! Each Person carries a `PersonKnowledge` component holding two bitsets:
//! `aware` (heard of it, no capacity cost, gossiped freely) and `learned`
//! (can perform/teach, costs complexity points, subset of `aware`). Capacity is
//! intelligence-driven; learning past capacity demotes the least-recently-used
//! Learned tech back to Aware-only.
//!
//! Discovery is driven per-action by `try_discover_from_action`, called from
//! the existing yield/combat hooks (gather, production, combat, plants).
//! The faction's collective `FactionTechs` is a derived projection of the
//! chief's `aware` bitset (see `faction::sync_faction_techs_from_chief_system`).
use bevy::prelude::*;

use super::skills::{SkillKind, Skills};
use super::stats::{modifier, Stats};
use super::technology::{
    complexity, tech_def, ActivityKind, TechId, TechTrigger, TECH_COUNT, TECH_TREE,
};

pub const KNOWLEDGE_SLOTS: usize = 64;

/// One game-day (3600 ticks at 20 Hz × 180s) of study per complexity point.
/// Cuneiform (complexity 6) takes 6 days of solo reading; a paleolithic tech
/// takes one day. Lectures and 1-on-1 teaching contribute at higher rates.
pub const STUDY_TICKS_PER_COMPLEXITY: u32 = 3600;

/// Per-person knowledge state.
///
/// `aware`: bit set if the person has *heard of* a tech. Free, gossipable.
/// `learned`: bit set if the person has mastered the tech. Costs complexity
/// points (see `complexity()`); always a subset of `aware`.
/// `learned_at`: last tick this tech was learned or refreshed (used or taught).
#[derive(Component, Clone, Debug)]
pub struct PersonKnowledge {
    pub aware: u64,
    pub learned: u64,
    pub learned_at: [u32; KNOWLEDGE_SLOTS],
    /// Sparse map TechId → progress ticks accumulated toward Learned. Cleared
    /// on successful learn or eviction. Used by Phase-2 reading/lecture/teach
    /// systems; the original passive `tech_teaching_system` and
    /// `try_discover_from_action` paths bypass it (they roll directly).
    pub study_progress: ahash::AHashMap<TechId, u32>,
}

impl Default for PersonKnowledge {
    fn default() -> Self {
        Self {
            aware: 0,
            learned: 0,
            learned_at: [0u32; KNOWLEDGE_SLOTS],
            study_progress: ahash::AHashMap::new(),
        }
    }
}

impl PersonKnowledge {
    /// Seed a brand-new agent with all Paleolithic techs both Aware and Learned.
    pub fn paleolithic_seed(now: u32) -> Self {
        let mut k = Self::default();
        for def in TECH_TREE.iter() {
            if matches!(def.era, super::technology::Era::Paleolithic) {
                k.aware |= 1u64 << def.id;
                k.learned |= 1u64 << def.id;
                k.learned_at[def.id as usize] = now;
            }
        }
        k
    }

    #[inline]
    pub fn is_aware(&self, id: TechId) -> bool {
        (self.aware >> id) & 1 != 0
    }

    #[inline]
    pub fn has_learned(&self, id: TechId) -> bool {
        (self.learned >> id) & 1 != 0
    }

    /// OR another agent's awareness into ours (gossip transfer).
    pub fn merge_awareness(&mut self, other_aware: u64) {
        self.aware |= other_aware;
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

    /// Attempt to add `id` to Learned. If capacity is exceeded, demote the
    /// least-recently-used Learned tech back to Aware-only and retry.
    /// Returns the demoted tech (if any) so callers can log it.
    /// No-op if the tech is already Learned (still refreshes recency).
    pub fn try_learn(&mut self, id: TechId, capacity: u16, now: u32) -> LearnOutcome {
        if self.has_learned(id) {
            self.learned_at[id as usize] = now;
            return LearnOutcome::AlreadyKnown;
        }
        let cost = complexity(id) as u16;
        let mut demoted: Option<TechId> = None;
        // Evict LRU until adding `id` fits under capacity. Each eviction
        // demotes one Learned tech; the awareness bit is retained.
        while self.complexity_used().saturating_add(cost) > capacity {
            let Some(victim) = self.lru_learned(id) else {
                // Nothing to evict (capacity is too small for even this one
                // tech). Refuse the learn rather than infinite-looping.
                return LearnOutcome::CapacityTooSmall;
            };
            self.learned &= !(1u64 << victim);
            self.learned_at[victim as usize] = 0;
            demoted = Some(victim);
        }
        self.aware |= 1u64 << id;
        self.learned |= 1u64 << id;
        self.learned_at[id as usize] = now;
        LearnOutcome::Learned { demoted }
    }

    /// LRU among currently-Learned techs, excluding `exclude` (the tech being
    /// added). Lowest `learned_at` wins; ties broken by lowest id.
    fn lru_learned(&self, exclude: TechId) -> Option<TechId> {
        let mut best: Option<(TechId, u32)> = None;
        for id in 0..TECH_COUNT as TechId {
            if id == exclude || !self.has_learned(id) {
                continue;
            }
            let t = self.learned_at[id as usize];
            match best {
                None => best = Some((id, t)),
                Some((_, bt)) if t < bt => best = Some((id, t)),
                _ => {}
            }
        }
        best.map(|(id, _)| id)
    }
}

/// Outcome of a `try_learn` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnOutcome {
    /// Newly learned. `demoted` is the LRU tech evicted to make room (if any).
    Learned { demoted: Option<TechId> },
    /// Already in the Learned set; recency was refreshed.
    AlreadyKnown,
    /// Capacity is too small to fit even this single tech (no eviction helps).
    /// Caller should leave the bitset unchanged.
    CapacityTooSmall,
}

/// Outcome of a single `add_study_progress` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudyOutcome {
    /// Progress accumulated, threshold not yet met.
    InProgress { progress: u32, threshold: u32 },
    /// Threshold reached; tech is now Learned.
    Learned { demoted: Option<TechId> },
    /// Threshold reached but capacity refused the learn.
    CapacityTooSmall,
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
    /// On reaching `study_threshold(tech)`, runs `try_learn` and clears
    /// progress. Returns the StudyOutcome so callers can log.
    pub fn add_study_progress(
        &mut self,
        tech: TechId,
        amount: u32,
        capacity: u16,
        now: u32,
    ) -> StudyOutcome {
        if self.has_learned(tech) {
            self.learned_at[tech as usize] = now;
            return StudyOutcome::AlreadyLearned;
        }
        // Awareness is free.
        self.aware |= 1u64 << tech;
        let entry = self.study_progress.entry(tech).or_insert(0);
        *entry = entry.saturating_add(amount);
        let threshold = study_threshold(tech);
        if *entry >= threshold {
            self.study_progress.remove(&tech);
            return match self.try_learn(tech, capacity, now) {
                LearnOutcome::Learned { demoted } => StudyOutcome::Learned { demoted },
                LearnOutcome::AlreadyKnown => StudyOutcome::AlreadyLearned,
                LearnOutcome::CapacityTooSmall => StudyOutcome::CapacityTooSmall,
            };
        }
        let progress = *entry;
        StudyOutcome::InProgress {
            progress,
            threshold,
        }
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
    }
}

/// Per-action discovery roll. For each tech whose triggers include `activity`
/// and whose prerequisites the person has personally Learned, roll
/// `base * (1 + int_mod) * (1 + skill_xp / 1000)`. On success, mark Learned.
/// Returns the discovered tech id, if any.
pub fn try_discover_from_action(
    knowledge: &mut PersonKnowledge,
    stats: &Stats,
    skills: &Skills,
    activity: ActivityKind,
    capacity: u16,
    now: u32,
) -> Option<TechId> {
    let int_mod = modifier(stats.intelligence) as f32;
    let int_scale = 1.0 + (int_mod * 0.1).max(-0.4);
    let skill = activity_to_skill(activity);
    let skill_xp = skills.get(skill) as f32;
    let skill_scale = 1.0 + (skill_xp / 1000.0).min(2.0);

    for def in TECH_TREE.iter() {
        if knowledge.has_learned(def.id) {
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
            // Discovery yields Learned directly. If capacity blocks it, the
            // LRU-eviction handles the trade-off.
            match knowledge.try_learn(def.id, capacity, now) {
                LearnOutcome::Learned { .. } => return Some(def.id),
                _ => {}
            }
        }
    }
    None
}

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

/// Tech-awareness gossip between agents within 3 tiles whose goal is
/// `Socialize`. Awareness is free (single bit) and propagates only between
/// socializing agents; mastery (Learned) only spreads via the explicit
/// `tech_teaching_system` chance roll.
///
/// Lifted from the deleted `plan_gossip_system` which also gossiped
/// `KnownPlans` entries — that half is gone with the plan/ module retirement
/// in Phase 7. Runs in `SimulationSet::Economy` after
/// `conversation_memory_system`, before `tech_teaching_system`.
pub fn awareness_gossip_system(
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut q: Query<(
        Entity,
        &Transform,
        &super::goals::AgentGoal,
        &super::lod::LodLevel,
        &mut PersonKnowledge,
        Option<&mut super::memory::AgentMemory>,
    )>,
) {
    use super::goals::AgentGoal;
    use super::lod::LodLevel;

    // Snapshot each socializing agent's tech-awareness (aware|learned)
    // AND visited settlements (Pluralist Economy R8 follow-on). Both
    // OR-merge between adjacent socializing agents; gossip is free,
    // teaching (Learned) is the bottleneck via tech_teaching_system.
    let snapshots: ahash::AHashMap<
        Entity,
        (
            u64,
            Vec<crate::simulation::settlement::SettlementId>,
        ),
    > = q
        .iter()
        .filter(|(_, _, goal, lod, _, _)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .map(|(e, _, _, _, k, mem_opt)| {
            let aware = k.aware | k.learned;
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

    for (entity, transform, goal, lod, mut knowledge, mem_opt) in q.iter_mut() {
        if *lod == LodLevel::Dormant || !matches!(goal, AgentGoal::Socialize) {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE_LOCAL).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE_LOCAL).floor() as i32;
        let mut aware_received: u64 = 0;
        let mut settlements_received: ahash::AHashSet<
            crate::simulation::settlement::SettlementId,
        > = ahash::AHashSet::default();
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    if let Some((snap_aware, snap_settlements)) = snapshots.get(&other) {
                        aware_received |= snap_aware;
                        for sid in snap_settlements {
                            settlements_received.insert(*sid);
                        }
                    }
                }
            }
        }
        if aware_received != 0 {
            knowledge.merge_awareness(aware_received);
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
    )>,
) {
    use super::goals::AgentGoal;
    use super::lod::LodLevel;
    use super::person::Profession;
    use crate::simulation::shared_knowledge::KnowledgeTier;

    const TIER_PROMOTION_CADENCE: u64 = 200; // 10s game-time
    if clock.tick % TIER_PROMOTION_CADENCE != 0 {
        return;
    }

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
        .filter(|(_, _, goal, lod, _, _, _, _)| {
            matches!(goal, AgentGoal::Socialize) && **lod != LodLevel::Dormant
        })
        .filter_map(|(e, t, _, _, profession, fm, chief, hm)| {
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
    let mut household_to_settlement: ahash::AHashSet<(u32, crate::simulation::settlement::SettlementId)> =
        ahash::AHashSet::default();
    let mut settlement_to_faction: ahash::AHashSet<(crate::simulation::settlement::SettlementId, u32)> =
        ahash::AHashSet::default();
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
            let entries: Vec<((i32, i32), crate::simulation::memory::MemoryKind, crate::simulation::shared_knowledge::ResourceOwner)> = map
                .clusters
                .values()
                .filter_map(|c| {
                    let rep = c.representative_tiles.iter().find_map(|s| *s).unwrap_or(c.center);
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
            let entries: Vec<((i32, i32), crate::simulation::memory::MemoryKind, crate::simulation::shared_knowledge::ResourceOwner)> = map
                .clusters
                .values()
                .filter_map(|c| {
                    let rep = c.representative_tiles.iter().find_map(|s| *s).unwrap_or(c.center);
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
    let snapshots: ahash::AHashMap<Entity, u64> = q
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
        let mut best_teach_set: u64 = 0;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                for &other in spatial.get(tx + dx, ty + dy) {
                    if other == entity {
                        continue;
                    }
                    let Some(&other_learned) = snapshots.get(&other) else {
                        continue;
                    };
                    // Teachable: teacher has Learned, student is Aware but not
                    // Learned. (`aware` is the shared awareness; the student
                    // needs to have heard of the tech first.)
                    let teachable = other_learned & knowledge.aware & !knowledge.learned;
                    if teachable.count_ones() > best_teach_set.count_ones() {
                        best_teach_set = teachable;
                    }
                }
            }
        }

        if best_teach_set == 0 {
            continue;
        }

        // Per-tick teach chance: very small base rate, scaled by intelligence
        // modifier. Even a brilliant student needs many social ticks to pick
        // up a complex technique.
        let int_scale = 1.0 + (modifier(stats.intelligence) as f32 * 0.15).max(-0.5);
        let chance = 0.004f32 * int_scale;
        if fastrand::f32() >= chance {
            continue;
        }

        // Pick the highest-complexity teachable tech (most valuable lesson).
        let mut chosen: Option<TechId> = None;
        let mut chosen_cx: u8 = 0;
        for id in 0..TECH_COUNT as TechId {
            if (best_teach_set >> id) & 1 == 0 {
                continue;
            }
            let cx = complexity(id);
            if cx > chosen_cx {
                chosen = Some(id);
                chosen_cx = cx;
            }
        }
        let Some(tech_id) = chosen else { continue };

        let capacity = capacity_for(stats);
        if let LearnOutcome::Learned { .. } = knowledge.try_learn(tech_id, capacity, now) {
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
        let capacity = capacity_for(stats);
        if let Some(tech_id) = try_discover_from_action(
            &mut knowledge,
            stats,
            skills,
            ev.activity,
            capacity,
            clock.tick as u32,
        ) {
            // Emit a tech-discovery activity entry for the player faction.
            if let Some(fm) = fm {
                if fm.faction_id == player.faction_id {
                    let def = tech_def(tech_id);
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
