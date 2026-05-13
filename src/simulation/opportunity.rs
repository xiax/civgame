use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;
use crate::simulation::faction::{FactionMember, FactionRegistry};
use crate::simulation::jobs::{JobBoard, JobId, JobKind};
use crate::simulation::medicine::Injury;
use crate::simulation::schedule::SimClock;
use crate::world::chunk::CHUNK_SIZE;
use crate::world::terrain::TILE_SIZE;

/// Structured choices produced by institutions and broad cache builders.
/// Scorers read this surface instead of each one scanning raw ECS state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpportunityKind {
    PaidJob,
    MaterialDeficit,
    FoodSource,
    CareNeed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpportunityPayload {
    PaidJob {
        job_id: JobId,
        kind: JobKind,
        reward: f32,
        target: Option<ResourceId>,
    },
    MaterialDeficit {
        resource_id: ResourceId,
        deficit: u32,
    },
    FoodSource {
        qty: u32,
    },
    CareNeed {
        patient: Entity,
        severity: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opportunity {
    pub kind: OpportunityKind,
    pub tile: (i32, i32),
    pub faction_id: u32,
    pub payload: OpportunityPayload,
    pub expires_tick: u64,
}

#[derive(Resource, Default, Debug)]
pub struct OpportunityIndex {
    entries: Vec<Opportunity>,
    by_kind: AHashMap<OpportunityKind, Vec<usize>>,
    by_faction_kind: AHashMap<(u32, OpportunityKind), Vec<usize>>,
    by_region_kind: AHashMap<((i32, i32), OpportunityKind), Vec<usize>>,
    pub rebuilt_at_tick: u64,
}

impl OpportunityIndex {
    pub fn clear(&mut self, now: u64) {
        self.entries.clear();
        self.by_kind.clear();
        self.by_faction_kind.clear();
        self.by_region_kind.clear();
        self.rebuilt_at_tick = now;
    }

    pub fn push(&mut self, opportunity: Opportunity) {
        let idx = self.entries.len();
        let region = region_key(opportunity.tile);
        self.by_kind.entry(opportunity.kind).or_default().push(idx);
        self.by_faction_kind
            .entry((opportunity.faction_id, opportunity.kind))
            .or_default()
            .push(idx);
        self.by_region_kind
            .entry((region, opportunity.kind))
            .or_default()
            .push(idx);
        self.entries.push(opportunity);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter_kind(&self, kind: OpportunityKind) -> impl Iterator<Item = &Opportunity> + '_ {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flat_map(|idxs| idxs.iter())
            .filter_map(|&idx| self.entries.get(idx))
    }

    pub fn iter_kind_for_faction(
        &self,
        faction_id: u32,
        kind: OpportunityKind,
    ) -> impl Iterator<Item = &Opportunity> + '_ {
        self.by_faction_kind
            .get(&(faction_id, kind))
            .into_iter()
            .flat_map(|idxs| idxs.iter())
            .filter_map(|&idx| self.entries.get(idx))
    }

    pub fn iter_kind_for_region(
        &self,
        tile: (i32, i32),
        kind: OpportunityKind,
    ) -> impl Iterator<Item = &Opportunity> + '_ {
        let region = region_key(tile);
        self.by_region_kind
            .get(&(region, kind))
            .into_iter()
            .flat_map(|idxs| idxs.iter())
            .filter_map(|&idx| self.entries.get(idx))
    }

    pub fn evict_expired(&mut self, now: u64) {
        if self.entries.iter().all(|o| o.expires_tick > now) {
            return;
        }
        let live: Vec<Opportunity> = self
            .entries
            .iter()
            .copied()
            .filter(|o| o.expires_tick > now)
            .collect();
        self.clear(now);
        for opportunity in live {
            self.push(opportunity);
        }
    }
}

fn region_key(tile: (i32, i32)) -> (i32, i32) {
    (
        tile.0.div_euclid(CHUNK_SIZE as i32),
        tile.1.div_euclid(CHUNK_SIZE as i32),
    )
}

const OPPORTUNITY_REBUILD_CADENCE: u64 = 20;
const OPPORTUNITY_TTL: u64 = OPPORTUNITY_REBUILD_CADENCE * 2;

pub fn rebuild_opportunity_index_system(
    clock: Res<SimClock>,
    board: Res<JobBoard>,
    registry: Res<FactionRegistry>,
    injured: Query<(Entity, &FactionMember, &Transform, &Injury)>,
    mut index: ResMut<OpportunityIndex>,
) {
    let now = clock.tick;
    if now != 0
        && index.rebuilt_at_tick != 0
        && now.saturating_sub(index.rebuilt_at_tick) < OPPORTUNITY_REBUILD_CADENCE
    {
        index.evict_expired(now);
        return;
    }

    index.clear(now);
    let expires_tick = now + OPPORTUNITY_TTL;

    for postings in board.postings.values() {
        for posting in postings {
            if posting.reward <= 0.0 || !posting.claimants.is_empty() {
                continue;
            }
            if posting
                .expiry_tick
                .map(|expiry| now > expiry as u64)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(faction) = registry.factions.get(&posting.faction_id) else {
                continue;
            };
            index.push(Opportunity {
                kind: OpportunityKind::PaidJob,
                tile: faction.home_tile,
                faction_id: posting.faction_id,
                payload: OpportunityPayload::PaidJob {
                    job_id: posting.id,
                    kind: posting.kind,
                    reward: posting.reward,
                    target: posting.progress.target_rid(),
                },
                expires_tick,
            });
        }
    }

    for (&faction_id, faction) in registry.factions.iter() {
        let food_qty = faction.storage.food_total() as u32;
        if food_qty > 0 {
            index.push(Opportunity {
                kind: OpportunityKind::FoodSource,
                tile: faction.home_tile,
                faction_id,
                payload: OpportunityPayload::FoodSource { qty: food_qty },
                expires_tick,
            });
        }

        let mut material_targets: AHashMap<ResourceId, u32> = AHashMap::default();
        for (&rid, &target) in faction.material_targets.iter() {
            material_targets.insert(rid, target);
        }
        for (&rid, &demand) in faction.resource_demand.iter() {
            let entry = material_targets.entry(rid).or_insert(0);
            *entry = (*entry).max(demand);
        }
        for (resource_id, target) in material_targets {
            let stored = faction.storage.stock_of(resource_id);
            let deficit = target.saturating_sub(stored);
            if deficit == 0 {
                continue;
            }
            index.push(Opportunity {
                kind: OpportunityKind::MaterialDeficit,
                tile: faction.home_tile,
                faction_id,
                payload: OpportunityPayload::MaterialDeficit {
                    resource_id,
                    deficit,
                },
                expires_tick,
            });
        }
    }

    for (patient, member, transform, injury) in injured.iter() {
        let tile = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        index.push(Opportunity {
            kind: OpportunityKind::CareNeed,
            tile,
            faction_id: member.faction_id,
            payload: OpportunityPayload::CareNeed {
                patient,
                severity: injury.severity,
            },
            expires_tick,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_buckets_by_kind_faction_and_region() {
        let mut index = OpportunityIndex::default();
        index.push(Opportunity {
            kind: OpportunityKind::PaidJob,
            tile: (33, 1),
            faction_id: 7,
            payload: OpportunityPayload::PaidJob {
                job_id: 1,
                kind: JobKind::Craft,
                reward: 10.0,
                target: None,
            },
            expires_tick: 10,
        });

        assert_eq!(index.iter_kind(OpportunityKind::PaidJob).count(), 1);
        assert_eq!(
            index
                .iter_kind_for_faction(7, OpportunityKind::PaidJob)
                .count(),
            1
        );
        assert_eq!(
            index
                .iter_kind_for_region((40, 2), OpportunityKind::PaidJob)
                .count(),
            1
        );
    }

    #[test]
    fn evict_expired_rebuilds_secondary_indexes() {
        let mut index = OpportunityIndex::default();
        index.push(Opportunity {
            kind: OpportunityKind::CareNeed,
            tile: (0, 0),
            faction_id: 1,
            payload: OpportunityPayload::CareNeed {
                patient: Entity::from_raw(1),
                severity: 20,
            },
            expires_tick: 5,
        });
        index.evict_expired(5);
        assert!(index.is_empty());
        assert_eq!(
            index
                .iter_kind_for_faction(1, OpportunityKind::CareNeed)
                .count(),
            0
        );
    }
}
