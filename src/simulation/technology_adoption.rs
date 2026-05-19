//! Community-level technology adoption.
//!
//! `PersonKnowledge` is the canonical per-person record (Aware / Learned /
//! study_progress). This module layers a faction-level `AdoptionStage` on
//! top of *real practice and infrastructure* — knowledge that lives only
//! in one chief's head is `Rumored`, not Adopted; bronze beds appear only
//! when there's a workbench, a learned smith, and recent successful crafts.
//!
//! Three helper APIs split the gating surface so call sites stop conflating
//! "the chief heard of X" with "the village builds X with X":
//!
//! - [`can_direct_tech`] — chief-Aware. Planning / posting authority.
//! - [`community_has_adopted`] — stage ≥ `Adopted`. Civic gates, tier picks.
//! - [`worker_can_perform`] — per-person `has_learned`. Task execution.
//!
//! `derive_tech_adoption_system` recomputes the per-faction stage array on a
//! coarse Economy cadence (every `TICKS_PER_DAY / 4 = 900` ticks). Cheap: one
//! O(members) member pass per faction + 44 stage evals using cached
//! aggregates. Phase 4 will add stage-downgrade (decay).

use ahash::AHashMap;
use bevy::prelude::*;
use std::collections::VecDeque;

use crate::simulation::capital::{WorkshopKind, WorkshopOwnership};
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::knowledge::PersonKnowledge;
use crate::simulation::schedule::SimClock;
use crate::simulation::technology::{
    TechId, ANIMAL_HUSBANDRY, ARD_PLOW, BONE_TOOLS, BOW_AND_ARROW, BRIDGE_BUILDING, BRONZE_CASTING,
    BRONZE_TOOLS, BRONZE_WEAPONS, CITY_STATE_ORG, COPPER_TOOLS, COPPER_WORKING, CROP_CULTIVATION,
    CUNEIFORM_WRITING, DAM_BUILDING, DOG_DOMESTICATION, DRIED_MEAT, DUGOUT_CANOE, FERMENTATION,
    FIRED_POTTERY,
    FIRE_MAKING, FISHING, FLINT_KNAPPING, FOOD_SMOKING, GRANARY, HORSEBACK_RIDING, HORSE_TAMING,
    HUNTING_SPEAR, IRRIGATION, LOG_RAFT, LONG_DIST_TRADE, LOOM_WEAVING, LUNAR_CALENDAR,
    MICROLITHIC_TOOLS, MONUMENTAL_BUILDING, OCHRE_PAINTING, OX_CART, PERM_SETTLEMENT,
    PORTABLE_DWELLINGS, POTTERS_WHEEL, PROFESSIONAL_ARMY, SACRED_RITUAL, SADDLE_QUERN, SCALE_ARMOR,
    TALLY_MARKS, TECH_COUNT, TECH_TREE, TIN_PROSPECTING, WAR_CHARIOT, WELL_DIGGING,
};

pub const TICKS_PER_DAY: u32 = 3600;
pub const TICKS_PER_GAME_YEAR: u32 = TICKS_PER_DAY * 20;
pub const ADOPTION_DERIVE_CADENCE: u32 = TICKS_PER_DAY / 4; // every 900 ticks

/// How "Adopted" a tech is across a faction. Stored as a packed `u8` per
/// tech on `FactionData.tech_adoption`. Order matters — `as u8` comparisons
/// are used elsewhere to gate (`stage >= AdoptionStage::Adopted`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdoptionStage {
    #[default]
    Unknown = 0,
    Rumored = 1,
    Demonstrated = 2,
    Practiced = 3,
    Adopted = 4,
    Institutionalized = 5,
}

impl AdoptionStage {
    pub fn name(self) -> &'static str {
        match self {
            AdoptionStage::Unknown => "Unknown",
            AdoptionStage::Rumored => "Rumored",
            AdoptionStage::Demonstrated => "Demonstrated",
            AdoptionStage::Practiced => "Practiced",
            AdoptionStage::Adopted => "Adopted",
            AdoptionStage::Institutionalized => "Institutionalized",
        }
    }
}

/// What kind of social / infrastructural footprint a tech needs to be
/// considered "adopted." Drives threshold rules in [`derive_stage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptionScale {
    /// Used by an individual — hunting spear, horseback riding.
    Personal,
    /// Spreads through family practice — pottery, smoking, weaving.
    Household,
    /// Broad seasonal practice across the band — cropping, animal husbandry.
    Subsistence,
    /// Requires a trained specialist + a station — smithing, metallurgy.
    Specialist,
    /// Equipment + animals + repeated deployment — chariotry, mounted war.
    MilitaryTransport,
    /// Requires officials, scale, records, civic buildings — writing,
    /// city-state organisation, monumental construction.
    Institutional,
}

/// Classify every tech in the static `TECH_TREE`. Unmapped techs default to
/// `Household` — the safest middle of the road. Keep this in sync with the
/// scale-mapping documentation in `plans/knowledge_based_technology_adoption.md`.
pub fn tech_scale(tech: TechId) -> AdoptionScale {
    match tech {
        // Personal — used by an individual; mastery sits in one body.
        HUNTING_SPEAR | BOW_AND_ARROW | HORSEBACK_RIDING | OCHRE_PAINTING => {
            AdoptionScale::Personal
        }
        // Specialist — needs a station + trained hands.
        FLINT_KNAPPING | MICROLITHIC_TOOLS | BONE_TOOLS | FIRED_POTTERY | POTTERS_WHEEL
        | LOOM_WEAVING | SADDLE_QUERN | COPPER_WORKING | COPPER_TOOLS | TIN_PROSPECTING
        | BRONZE_CASTING | BRONZE_TOOLS | BRONZE_WEAPONS | SCALE_ARMOR | ARD_PLOW => {
            AdoptionScale::Specialist
        }
        // Subsistence — broad band-wide seasonal practice.
        FIRE_MAKING | CROP_CULTIVATION | ANIMAL_HUSBANDRY | DOG_DOMESTICATION | FISHING
        | IRRIGATION | FERMENTATION | LOG_RAFT | DUGOUT_CANOE => AdoptionScale::Subsistence,
        // MilitaryTransport — equipment + animals + drilled deployment.
        HORSE_TAMING | WAR_CHARIOT | PROFESSIONAL_ARMY => AdoptionScale::MilitaryTransport,
        // Institutional — requires civic scale + officials + buildings.
        PERM_SETTLEMENT | GRANARY | SACRED_RITUAL | LONG_DIST_TRADE | TALLY_MARKS
        | CUNEIFORM_WRITING | CITY_STATE_ORG | MONUMENTAL_BUILDING | LUNAR_CALENDAR | OX_CART
        | PORTABLE_DWELLINGS | BRIDGE_BUILDING | WELL_DIGGING | DAM_BUILDING => {
            AdoptionScale::Institutional
        }
        // Household default catches FOOD_SMOKING, DRIED_MEAT, and any future
        // additions until they're explicitly classified.
        FOOD_SMOKING | DRIED_MEAT => AdoptionScale::Household,
        _ => AdoptionScale::Household,
    }
}

/// Per-(faction, tech) ring buffer of recent uses. Drives the "recent
/// successful crafts / deployments" half of `Adopted` thresholds.
/// `RECENT_TECH_USE_CAP = 8`; entries older than `RECENT_TECH_USE_TTL_TICKS`
/// (60 game-days) are dropped on read.
#[derive(Clone, Debug, Default)]
pub struct RecentTechUse {
    /// Ticks at which the tech was successfully exercised. Oldest first.
    pub stamps: VecDeque<u32>,
}

pub const RECENT_TECH_USE_CAP: usize = 8;
pub const RECENT_TECH_USE_TTL_TICKS: u32 = TICKS_PER_DAY * 60;

impl RecentTechUse {
    pub fn push(&mut self, now: u32) {
        self.stamps.push_back(now);
        while self.stamps.len() > RECENT_TECH_USE_CAP {
            self.stamps.pop_front();
        }
    }

    pub fn count_within(&self, now: u32, window_ticks: u32) -> usize {
        let cutoff = now.saturating_sub(window_ticks);
        self.stamps.iter().filter(|&&t| t >= cutoff).count()
    }

    pub fn newest(&self) -> Option<u32> {
        self.stamps.back().copied()
    }

    pub fn prune(&mut self, now: u32) {
        let cutoff = now.saturating_sub(RECENT_TECH_USE_TTL_TICKS);
        while let Some(&front) = self.stamps.front() {
            if front < cutoff {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Aggregate counts over the faction's members for a given tech. Cached
/// once per `derive_tech_adoption_system` pass and reused across all 44
/// tech evaluations.
#[derive(Clone, Copy, Debug, Default)]
pub struct FactionMemberAggregate {
    pub members: u32,
    pub adults: u32,
}

/// Per-tech counts pulled from the aggregate scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerTechCounts {
    pub aware: u32,
    pub learned: u32,
}

/// === Helpers (the three-bucket public API) ===

/// Chief-Aware. Planner / posting authority. A chief who's only heard of
/// bronze can still post a contract; a worker still needs `has_learned` to
/// claim it.
#[inline]
pub fn can_direct_tech(faction: &crate::simulation::faction::FactionData, tech: TechId) -> bool {
    faction.techs.has(tech)
}

// `community_has_adopted` was deleted (sleepy-dove): civic/tier/build
// gates no longer consult community adoption. Use `FactionData::
// community_has` (→ `buildable_techs`, the poster-pool surface) instead.

/// Worker / individual can personally perform the tech.
#[inline]
pub fn worker_can_perform(knowledge: &PersonKnowledge, tech: TechId) -> bool {
    knowledge.has_learned(tech)
}

/// === Derivation ===

/// Look up the dominant adoption stage for `(faction, tech)` given the
/// aggregate counts, recent use buffer, and infrastructure flags. Pure
/// function — fully testable without an `App`.
pub fn derive_stage(
    scale: AdoptionScale,
    counts: PerTechCounts,
    agg: FactionMemberAggregate,
    chief_aware: bool,
    chief_learned: bool,
    prereqs_adopted: bool,
    has_station: bool,
    has_artifact: bool,
    has_civic_building: bool,
    recent: Option<&RecentTechUse>,
    stage_since_tick: u32,
    now: u32,
) -> AdoptionStage {
    let adults = agg.adults.max(1);
    let _ = agg.members;
    let recent_30d = recent
        .map(|r| r.count_within(now, TICKS_PER_DAY * 30) as u32)
        .unwrap_or(0);
    let recent_60d = recent
        .map(|r| r.count_within(now, TICKS_PER_DAY * 60) as u32)
        .unwrap_or(0);
    let last_use_tick = recent.and_then(|r| r.newest());
    let adopted_long_enough = now.saturating_sub(stage_since_tick) >= TICKS_PER_GAME_YEAR;

    if counts.aware == 0 && !chief_aware {
        return AdoptionStage::Unknown;
    }

    // "Broad-learning short-circuit": if every adult knows the tech, the
    // community is by definition doing it — even before any per-tech use
    // event has been recorded. This handles `seeded_through_era` (every
    // founder spawns Learned) without requiring artificial `record_tech_use`
    // priming, and matches the intuition that a band where everyone can
    // make fire is using fire today.
    let broadly_learned = counts.learned >= adults && counts.learned >= 1;

    match scale {
        AdoptionScale::Personal => {
            if counts.learned >= 1 {
                if adopted_long_enough {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if counts.aware >= 1 || chief_aware {
                AdoptionStage::Rumored
            } else {
                AdoptionStage::Unknown
            }
        }
        AdoptionScale::Household => {
            let threshold = ((adults as f32) * 0.20).ceil() as u32;
            let threshold = threshold.max(2);
            if broadly_learned || counts.learned >= threshold {
                if adopted_long_enough {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if counts.learned >= 1 {
                AdoptionStage::Practiced
            } else if has_artifact || counts.aware >= 2 {
                AdoptionStage::Demonstrated
            } else {
                AdoptionStage::Rumored
            }
        }
        AdoptionScale::Subsistence => {
            let households_used = recent_30d; // proxy: any recent use this season
            let threshold = ((adults as f32) * 0.30).ceil() as u32;
            if broadly_learned || (counts.learned >= 1 && households_used >= threshold) {
                if adopted_long_enough && has_artifact {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if counts.learned >= 1 && households_used >= 1 {
                AdoptionStage::Practiced
            } else if counts.learned >= 1 {
                AdoptionStage::Demonstrated
            } else if counts.aware >= 1 {
                AdoptionStage::Rumored
            } else {
                AdoptionStage::Unknown
            }
        }
        AdoptionScale::Specialist => {
            // Three paths to Adopted:
            //   (a) Trained-specialist path: ≥1 learned + station + recent
            //       successful crafts. The historical model — bronze beds
            //       only appear once a smith has worked the workbench.
            //   (b) Broad-practice path: every adult Learned.
            //   (c) Small-band-with-practitioner: ≤8 members + ≥1 learned.
            //       A founder band of 20 with 2 dedicated smiths IS doing
            //       smithing; it just doesn't have the workshop yet. The
            //       gate tightens as the band grows past city-state scale.
            let trained = counts.learned >= 1 && has_station && recent_30d >= 3;
            let small_band_practitioner = agg.members <= 8 && counts.learned >= 1;
            if trained || broadly_learned || small_band_practitioner {
                let practitioners_alive = counts.learned >= 2;
                if adopted_long_enough && practitioners_alive && has_station {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if counts.learned >= 1 && (has_station || last_use_tick.is_some()) {
                AdoptionStage::Practiced
            } else if counts.learned >= 1 {
                AdoptionStage::Demonstrated
            } else if counts.aware >= 1 {
                AdoptionStage::Rumored
            } else {
                AdoptionStage::Unknown
            }
        }
        AdoptionScale::MilitaryTransport => {
            if counts.learned >= 2 && recent_60d >= 3 {
                if adopted_long_enough {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if counts.learned >= 1 && has_artifact {
                AdoptionStage::Practiced
            } else if counts.learned >= 1 {
                AdoptionStage::Demonstrated
            } else if counts.aware >= 1 {
                AdoptionStage::Rumored
            } else {
                AdoptionStage::Unknown
            }
        }
        AdoptionScale::Institutional => {
            let scale_pop_ok = agg.members >= 8;
            // Two adoption paths for Institutional techs:
            //   (a) Civic-scale: chief learned + prereqs adopted + civic
            //       building present + population ≥ scale. The strict
            //       historical model — a city-state needs a city.
            //   (b) Broad-learning shortcut: the whole band has Learned
            //       it. Catches small founder bands seeded through an
            //       era (e.g. a Neolithic 4-person family that "has"
            //       permanent-settlement know-how even without a stone
            //       wall yet). Without this, tiny test fixtures and
            //       early-game bands can never reach Adopted.
            let civic_path = chief_learned && prereqs_adopted && has_civic_building && scale_pop_ok;
            let broad_path = chief_learned && prereqs_adopted && broadly_learned;
            if civic_path || broad_path {
                let preserved = has_artifact || counts.learned >= 2;
                if adopted_long_enough && preserved {
                    AdoptionStage::Institutionalized
                } else {
                    AdoptionStage::Adopted
                }
            } else if chief_learned && prereqs_adopted {
                AdoptionStage::Practiced
            } else if has_civic_building || counts.learned >= 1 {
                AdoptionStage::Demonstrated
            } else if counts.aware >= 1 || chief_aware {
                AdoptionStage::Rumored
            } else {
                AdoptionStage::Unknown
            }
        }
    }
}

/// Map a `TechId` to the `WorkshopKind` that satisfies its "station present"
/// gate, if any. `None` = no station required.
pub fn required_station(tech: TechId) -> Option<WorkshopKind> {
    match tech {
        FLINT_KNAPPING | MICROLITHIC_TOOLS | BONE_TOOLS | COPPER_WORKING | COPPER_TOOLS
        | TIN_PROSPECTING | BRONZE_CASTING | BRONZE_TOOLS | BRONZE_WEAPONS | SCALE_ARMOR
        | CUNEIFORM_WRITING => Some(WorkshopKind::Workbench),
        LOOM_WEAVING => Some(WorkshopKind::Loom),
        GRANARY => Some(WorkshopKind::Granary),
        SACRED_RITUAL => Some(WorkshopKind::Shrine),
        LONG_DIST_TRADE => Some(WorkshopKind::Market),
        PROFESSIONAL_ARMY => Some(WorkshopKind::Barracks),
        MONUMENTAL_BUILDING | CITY_STATE_ORG => Some(WorkshopKind::Monument),
        _ => None,
    }
}

/// The Economy-schedule system that recomputes `FactionData.tech_adoption`
/// from member knowledges + workshop ownership + recent use. Runs every
/// `ADOPTION_DERIVE_CADENCE` ticks. Emits `TechAdopted` /
/// `TechInstitutionalized` activity-log entries on upgrade transitions
/// (downgrades stay silent — Phase 4's decay is intentionally quiet).
pub fn derive_tech_adoption_system(
    clock: Res<SimClock>,
    workshops: Res<WorkshopOwnership>,
    mut registry: ResMut<FactionRegistry>,
    members_q: Query<(&FactionMember, &PersonKnowledge)>,
    player: Res<crate::simulation::faction::PlayerFaction>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    if (clock.tick as u32) % ADOPTION_DERIVE_CADENCE != 0 {
        return;
    }
    let now = clock.tick as u32;

    let mut per_faction: AHashMap<u32, (FactionMemberAggregate, [PerTechCounts; TECH_COUNT])> =
        AHashMap::new();
    for (fm, knowledge) in members_q.iter() {
        let entry = per_faction.entry(fm.faction_id).or_insert_with(|| {
            (
                FactionMemberAggregate::default(),
                [PerTechCounts::default(); TECH_COUNT],
            )
        });
        entry.0.members += 1;
        entry.0.adults += 1;
        for id in 0..TECH_COUNT as TechId {
            let idx = id as usize;
            if knowledge.is_aware(id) {
                entry.1[idx].aware += 1;
            }
            if knowledge.has_learned(id) {
                entry.1[idx].learned += 1;
            }
        }
    }

    for (fid, faction) in registry.factions.iter_mut() {
        let (agg, counts) = match per_faction.get(fid) {
            Some(v) => *v,
            None => (
                FactionMemberAggregate::default(),
                [PerTechCounts::default(); TECH_COUNT],
            ),
        };
        let chief_techs = faction.techs.0;
        let workshops_owned = workshops.workshops_for(*fid);
        let has_workshop = |k: WorkshopKind| workshops_owned.iter().any(|w| w.kind == k);

        for entry in faction.recent_tech_use.values_mut() {
            entry.prune(now);
        }

        let mut new_stages: [AdoptionStage; TECH_COUNT] = faction.tech_adoption;
        // TECH_TREE is in roughly-topological order (a tech's prereqs sit
        // at lower TechIds). Read prereq state from `new_stages` so a
        // prereq adopted earlier in this pass propagates immediately —
        // otherwise PERM_SETTLEMENT couldn't reach Adopted on tick 0
        // even when CROP_CULTIVATION + FIRED_POTTERY both qualified.
        for def in TECH_TREE.iter() {
            let id = def.id;
            let idx = id as usize;
            let scale = tech_scale(id);
            let c = counts[idx];
            let chief_aware = chief_techs & (1u64 << id) != 0;
            let chief_learned = chief_aware
                && faction
                    .chief_entity
                    .and_then(|e| members_q.get(e).ok())
                    .map(|(_, k)| k.has_learned(id))
                    .unwrap_or(false);
            let prereqs_adopted = def
                .prerequisites
                .iter()
                .all(|&p| (new_stages[p as usize] as u8) >= (AdoptionStage::Adopted as u8));
            let has_station = required_station(id).map(has_workshop).unwrap_or(true);
            let has_civic = matches!(scale, AdoptionScale::Institutional,)
                && required_station(id).map(has_workshop).unwrap_or(true);
            let has_artifact = false; // Tablets / books not counted yet; future work.
            let recent = faction.recent_tech_use.get(&id);
            let stage_since = faction.stage_changed_at_tick[idx];

            new_stages[idx] = derive_stage(
                scale,
                c,
                agg,
                chief_aware,
                chief_learned,
                prereqs_adopted,
                has_station,
                has_artifact,
                has_civic,
                recent,
                stage_since,
                now,
            );
        }

        // Apply changes with a one-step-per-game-day decay cooldown.
        // Upgrades land immediately (a tech is "doing" the moment conditions
        // are met); downgrades are throttled so a single missed use can't
        // flick Adopted → Demonstrated in one pass.
        for idx in 0..TECH_COUNT {
            let prev = faction.tech_adoption[idx];
            let computed = new_stages[idx];
            let final_stage = if (computed as u8) < (prev as u8) {
                let elapsed = now.saturating_sub(faction.stage_changed_at_tick[idx]);
                if elapsed < TICKS_PER_DAY {
                    prev
                } else {
                    let stepped = step_down(prev);
                    if (stepped as u8) > (computed as u8) {
                        stepped
                    } else {
                        computed
                    }
                }
            } else {
                computed
            };
            if final_stage != prev {
                faction.stage_changed_at_tick[idx] = now;
                faction.tech_adoption[idx] = final_stage;
                // Emit player-faction stage-up notifications for the two
                // load-bearing transitions. Downgrades stay silent so a
                // brief Adopted → Practiced flicker during a bad season
                // doesn't spam the log.
                if *fid == player.faction_id && (final_stage as u8) > (prev as u8) {
                    let def = &TECH_TREE[idx];
                    match final_stage {
                        AdoptionStage::Adopted => {
                            activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                                tick: clock.tick,
                                actor: faction.chief_entity.unwrap_or(Entity::PLACEHOLDER),
                                faction_id: *fid,
                                kind: crate::ui::activity_log::ActivityEntryKind::TechAdopted {
                                    tech_name: def.name,
                                    era_name: def.era.name(),
                                },
                            });
                        }
                        AdoptionStage::Institutionalized => {
                            activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                                tick: clock.tick,
                                actor: faction.chief_entity.unwrap_or(Entity::PLACEHOLDER),
                                faction_id: *fid,
                                kind:
                                    crate::ui::activity_log::ActivityEntryKind::TechInstitutionalized {
                                        tech_name: def.name,
                                        era_name: def.era.name(),
                                    },
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Returns the next-lower stage (saturating at `Unknown`).
fn step_down(stage: AdoptionStage) -> AdoptionStage {
    match stage {
        AdoptionStage::Unknown => AdoptionStage::Unknown,
        AdoptionStage::Rumored => AdoptionStage::Unknown,
        AdoptionStage::Demonstrated => AdoptionStage::Rumored,
        AdoptionStage::Practiced => AdoptionStage::Demonstrated,
        AdoptionStage::Adopted => AdoptionStage::Practiced,
        AdoptionStage::Institutionalized => AdoptionStage::Adopted,
    }
}

/// The faction's construction-tech surface. **No longer derived from
/// community adoption** — it returns `faction.buildable_techs`, the
/// poster-pool union of resident chief + architect `Learned`, rewritten
/// every tick by `construction::refresh_construction_poster_pool_system`.
/// Kept as a free function so the many existing `best_X_for(&FactionTechs)`
/// call sites need no signature churn; they now uniformly see the one
/// poster-pool surface.
#[inline]
pub fn community_adoption_bitset(
    faction: &crate::simulation::faction::FactionData,
) -> crate::simulation::faction::FactionTechs {
    faction.buildable_techs
}

// `seed_prime_tech_adoption_system` was deleted (sleepy-dove): nothing
// gates on community *adoption* any more. The construction-tech surface
// is `faction.buildable_techs`, written every tick (and once at OnEnter)
// by `construction::refresh_construction_poster_pool_system` from
// resident chief + architect Learned. `tech_adoption` is now display /
// analytics only (`ui/tech_panel`, activity log).

/// Stamp `(faction, tech, now)` into the recent-use ring. Call from craft /
/// hunt / build executors whenever the tech is successfully exercised.
pub fn record_tech_use(
    faction: &mut crate::simulation::faction::FactionData,
    tech: TechId,
    now: u32,
) {
    let entry = faction.recent_tech_use.entry(tech).or_default();
    entry.push(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg(members: u32) -> FactionMemberAggregate {
        FactionMemberAggregate {
            members,
            adults: members,
        }
    }

    fn counts(learned: u32, aware: u32) -> PerTechCounts {
        PerTechCounts { learned, aware }
    }

    #[test]
    fn personal_one_learned_is_adopted() {
        let s = derive_stage(
            AdoptionScale::Personal,
            counts(1, 1),
            agg(5),
            true,
            true,
            true,
            false,
            false,
            false,
            None,
            0,
            100,
        );
        assert_eq!(s, AdoptionStage::Adopted);
    }

    #[test]
    fn specialist_needs_station_and_recent_use() {
        // No station → at most Practiced (or Demonstrated without recent use).
        let no_station = derive_stage(
            AdoptionScale::Specialist,
            counts(1, 1),
            agg(10),
            true,
            true,
            true,
            false, // no station
            false,
            false,
            None,
            0,
            100,
        );
        assert_eq!(no_station, AdoptionStage::Demonstrated);

        let mut recent = RecentTechUse::default();
        recent.push(100);
        recent.push(200);
        recent.push(300);
        let full = derive_stage(
            AdoptionScale::Specialist,
            counts(1, 1),
            agg(10),
            true,
            true,
            true,
            true,
            false,
            false,
            Some(&recent),
            0,
            400,
        );
        assert_eq!(full, AdoptionStage::Adopted);
    }

    #[test]
    fn institutional_needs_population_and_civic() {
        let small = derive_stage(
            AdoptionScale::Institutional,
            counts(1, 1),
            agg(4), // below population threshold
            true,
            true,
            true,
            true,
            false,
            true,
            None,
            0,
            100,
        );
        assert!(small < AdoptionStage::Adopted);

        let big = derive_stage(
            AdoptionScale::Institutional,
            counts(2, 2),
            agg(10),
            true,
            true,
            true,
            true,
            false,
            true,
            None,
            0,
            100,
        );
        assert_eq!(big, AdoptionStage::Adopted);
    }

    #[test]
    fn step_down_walks_one_stage_at_a_time() {
        assert_eq!(
            step_down(AdoptionStage::Institutionalized),
            AdoptionStage::Adopted
        );
        assert_eq!(step_down(AdoptionStage::Adopted), AdoptionStage::Practiced);
        assert_eq!(
            step_down(AdoptionStage::Practiced),
            AdoptionStage::Demonstrated
        );
        assert_eq!(
            step_down(AdoptionStage::Demonstrated),
            AdoptionStage::Rumored
        );
        assert_eq!(step_down(AdoptionStage::Rumored), AdoptionStage::Unknown);
        assert_eq!(step_down(AdoptionStage::Unknown), AdoptionStage::Unknown);
    }

    #[test]
    fn specialist_decays_to_demonstrated_when_practitioners_die() {
        // Faction had a learned smith; now all dead. With recent_use stale
        // and counts.learned == 0, derive_stage returns Demonstrated... no
        // wait, with learned=0 it falls into the no-learned arms.
        let s = derive_stage(
            AdoptionScale::Specialist,
            counts(0, 1), // 1 aware, 0 learned
            agg(10),
            true,
            false,
            true,
            false, // station gone
            false,
            false,
            None,
            0,
            TICKS_PER_GAME_YEAR * 2,
        );
        // Only Aware remains → Rumored.
        assert_eq!(s, AdoptionStage::Rumored);
    }

    #[test]
    fn bridge_building_is_institutional() {
        assert_eq!(tech_scale(BRIDGE_BUILDING), AdoptionScale::Institutional);
    }

    #[test]
    fn bridge_building_tech_def_chalcolithic_with_prereqs() {
        let def = crate::simulation::technology::tech_def(BRIDGE_BUILDING);
        assert_eq!(def.era, crate::simulation::technology::Era::Chalcolithic);
        // Must require permanent settlement, dugout canoe, and copper tools.
        let mut prereqs: Vec<TechId> = def.prerequisites.to_vec();
        prereqs.sort_unstable();
        let mut expected = vec![PERM_SETTLEMENT, DUGOUT_CANOE, COPPER_TOOLS];
        expected.sort_unstable();
        assert_eq!(prereqs, expected);
    }

    #[test]
    fn unknown_when_nobody_aware() {
        let s = derive_stage(
            AdoptionScale::Household,
            counts(0, 0),
            agg(5),
            false,
            false,
            false,
            false,
            false,
            false,
            None,
            0,
            100,
        );
        assert_eq!(s, AdoptionStage::Unknown);
    }
}
