//! Territorial trespass detection + handling.
//!
//! Pipeline:
//! 1. `trespass_detection_system` — Sequential, after the spatial-index
//!    sync. Walks `Changed<Transform>` Persons; per-pair throttled. When
//!    an indexed mover enters a tile owned by another faction
//!    (`TerritoryMap`), emits `TrespassEvent`.
//! 2. `trespass_handling_system` — Economy. Drains events, records
//!    ledger incidents, escalates per (intruder, owner) state, and
//!    populates `TerritoryDefenseQueue` for war / repeated violators.
//!
//! Throttling: per-pair `TrespassCooldown` map prevents one walking
//! agent from re-firing every tile-step.

use crate::collections::AHashMap;
use bevy::prelude::*;
use std::collections::VecDeque;

use crate::simulation::access_grant::{
    classify_intent, permits, AccessGrantTable, IntruderIntent,
};
use crate::simulation::diplomacy::{
    record_incident, DiplomacyLedger, IncidentKind, TreatyKind, TreatySet,
};
use crate::simulation::diplomatic_personality::DiplomaticPersonality;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::person::{Drafted, Person, Profession};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{Settlement, SettlementMap};
use crate::simulation::territory::TerritoryMap;
use crate::world::seasons::{Calendar, TICKS_PER_DAY};
use crate::world::terrain::TILE_SIZE;

/// Per-(intruder-faction, owner-faction) cooldown ticks between
/// trespass events. 200 ticks ≈ 10 s real-time at 20 Hz / ~3 game-min.
pub const TRESPASS_COOLDOWN_TICKS: u64 = 200;

/// Ticks an entry stays in `TerritoryDefenseQueue` before expiry.
/// Quarter-day matches the defender-draft cadence.
pub const DEFENSE_TARGET_TTL: u64 = (TICKS_PER_DAY / 4) as u64;

/// Max queued defense targets per faction. Drops oldest on push.
pub const DEFENSE_QUEUE_CAP: usize = 8;

#[derive(Event, Debug, Clone)]
pub struct TrespassEvent {
    pub intruder: Entity,
    pub intruder_faction: u32,
    pub owner_faction: u32,
    pub tile: (i32, i32),
}

/// Per-(intruder-faction, owner-faction) state machine. Drives the
/// warning → grievance → attack escalation across multiple incidents.
#[derive(Clone, Copy, Default, Debug)]
pub struct TrespassPairState {
    pub last_event_tick: u64,
    pub last_handled_tick: u64,
    pub incidents_today: u8,
}

#[derive(Resource, Default)]
pub struct TrespassRegistry {
    /// (intruder_faction, owner_faction) → state. Throttle + escalation.
    pub by_pair: AHashMap<(u32, u32), TrespassPairState>,
}

/// One pending defense target per faction. Drained by the defender-
/// drafting system + HTN `DefendTerritoryMethod`.
#[derive(Clone, Copy, Debug)]
pub struct DefenseTarget {
    pub tile: (i32, i32),
    pub intruder: Entity,
    pub intruder_faction: u32,
    pub expires_tick: u64,
}

#[derive(Resource, Default)]
pub struct TerritoryDefenseQueue {
    pub by_faction: AHashMap<u32, VecDeque<DefenseTarget>>,
}

impl TerritoryDefenseQueue {
    pub fn push(&mut self, owner_faction: u32, target: DefenseTarget) {
        let q = self.by_faction.entry(owner_faction).or_default();
        if q.len() >= DEFENSE_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(target);
    }

    pub fn peek_nearest(
        &self,
        owner_faction: u32,
        from_tile: (i32, i32),
    ) -> Option<DefenseTarget> {
        self.by_faction.get(&owner_faction).and_then(|q| {
            q.iter()
                .min_by_key(|t| {
                    let dx = (t.tile.0 - from_tile.0).abs();
                    let dy = (t.tile.1 - from_tile.1).abs();
                    dx.max(dy)
                })
                .copied()
        })
    }

    pub fn evict_expired(&mut self, now: u64) {
        for q in self.by_faction.values_mut() {
            q.retain(|t| t.expires_tick > now);
        }
        self.by_faction.retain(|_, q| !q.is_empty());
    }

    pub fn has_target(&self, owner_faction: u32) -> bool {
        self.by_faction
            .get(&owner_faction)
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }
}

/// Pure helper: classify whether an intruder's presence on owner's
/// territory is legal under treaties + grant table + intent (Smart-
/// diplomacy P2). Same-root always allowed; war always hostile.
/// Otherwise, defers to `permits` against `grants`; non-permitted
/// neutrals become `Neutral` (warn-then-escalate).
///
/// `legacy_is_trespass` (treaty-only) remains for the pure test
/// surface.
pub fn classify_trespass(
    treaties: TreatySet,
    same_root: bool,
    grants: &[crate::simulation::access_grant::AccessGrant],
    intent: IntruderIntent,
    tile: (i32, i32),
    settlements: &[(crate::simulation::settlement::SettlementId, (i32, i32))],
    season: crate::world::seasons::Season,
) -> TrespassClassification {
    if same_root {
        return TrespassClassification::Allowed;
    }
    if treaties.has(TreatyKind::War) {
        return TrespassClassification::Hostile;
    }
    if permits(grants, intent, tile, settlements, season) {
        return TrespassClassification::Allowed;
    }
    TrespassClassification::Neutral
}

/// Legacy treaty-only classifier kept for pure-fn tests + any code path
/// that doesn't have the grant table in hand. Same shape as the P1
/// version, minus the P2 intent filter.
pub fn is_trespass(
    treaties: TreatySet,
    same_root: bool,
) -> TrespassClassification {
    if same_root {
        return TrespassClassification::Allowed;
    }
    if treaties.has(TreatyKind::Alliance)
        || treaties.has(TreatyKind::NonAggression)
        || treaties.has(TreatyKind::TradePact)
    {
        return TrespassClassification::Allowed;
    }
    if treaties.has(TreatyKind::War) {
        TrespassClassification::Hostile
    } else {
        TrespassClassification::Neutral
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrespassClassification {
    Allowed,
    Neutral,
    Hostile,
}

/// Sequential, after `sync_indexed_after_move_system`. Walks `Person`s
/// whose `Transform` changed and emits one `TrespassEvent` per
/// (intruder, owner_faction) cooldown window.
pub fn trespass_detection_system(
    clock: Res<SimClock>,
    territory: Res<TerritoryMap>,
    ledger: Res<DiplomacyLedger>,
    registry: Res<FactionRegistry>,
    grants: Res<AccessGrantTable>,
    calendar: Res<Calendar>,
    settlement_map: Res<SettlementMap>,
    settlements_q: Query<&Settlement>,
    mut state: ResMut<TrespassRegistry>,
    mut events: EventWriter<TrespassEvent>,
    persons: Query<
        (
            Entity,
            &Transform,
            &FactionMember,
            Option<&Drafted>,
            Option<&Profession>,
        ),
        (With<Person>, Changed<Transform>),
    >,
) {
    let now = clock.tick;
    for (entity, transform, member, drafted, profession) in persons.iter() {
        let intruder_fid = member.faction_id;
        if intruder_fid == SOLO {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let Some(owner_fid) = territory.owner_at((tx, ty)) else {
            continue;
        };
        if owner_fid == intruder_fid {
            continue;
        }
        let same_root = registry.root_faction(intruder_fid) == registry.root_faction(owner_fid);
        let treaties = ledger.treaties(intruder_fid, owner_fid);
        // Intent — drafted = Hostile; raid-party membership too (no
        // entity tag for that today, so derive from FactionData).
        let intruder_root = registry.root_faction(intruder_fid);
        let in_raid_party = registry
            .factions
            .get(&intruder_root)
            .map(|d| d.raid_party.contains(&entity))
            .unwrap_or(false);
        let is_trader = matches!(profession, Some(Profession::Trader));
        let home_is_mobile = registry
            .factions
            .get(&intruder_root)
            .map(|d| d.caps.home.is_mobile())
            .unwrap_or(false);
        let intent = classify_intent(drafted.is_some(), in_raid_party, is_trader, home_is_mobile);
        // Build the per-pair grant view + settlement view.
        let pair_grants = grants.grants(owner_fid, intruder_root);
        let owner_settlements: Vec<(crate::simulation::settlement::SettlementId, (i32, i32))> =
            settlement_map
                .for_faction(owner_fid)
                .iter()
                .filter_map(|sid| {
                    settlement_map
                        .by_id
                        .get(sid)
                        .and_then(|e| settlements_q.get(*e).ok())
                        .map(|s| (s.id, s.market_tile))
                })
                .collect();
        let class = classify_trespass(
            treaties,
            same_root,
            pair_grants,
            intent,
            (tx, ty),
            &owner_settlements,
            calendar.season,
        );
        if matches!(class, TrespassClassification::Allowed) {
            continue;
        }
        let key = (intruder_fid, owner_fid);
        let pair = state.by_pair.entry(key).or_default();
        if now.saturating_sub(pair.last_event_tick) < TRESPASS_COOLDOWN_TICKS {
            continue;
        }
        pair.last_event_tick = now;
        events.send(TrespassEvent {
            intruder: entity,
            intruder_faction: intruder_fid,
            owner_faction: owner_fid,
            tile: (tx, ty),
        });
    }
}

/// Economy, after detection. Folds events into ledger incidents and
/// queues defense targets.
pub fn trespass_handling_system(
    clock: Res<SimClock>,
    mut events: EventReader<TrespassEvent>,
    mut ledger: ResMut<DiplomacyLedger>,
    mut state: ResMut<TrespassRegistry>,
    mut queue: ResMut<TerritoryDefenseQueue>,
    registry: Res<FactionRegistry>,
    mut federation_map: ResMut<crate::simulation::federation::FederationMap>,
    mut log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
) {
    let now = clock.tick;
    queue.evict_expired(now);
    for ev in events.read() {
        let key = (ev.intruder_faction, ev.owner_faction);
        let pair = state.by_pair.entry(key).or_default();
        // Reset incident counter daily (cheap, no extra Local).
        let day_window = TICKS_PER_DAY as u64;
        if now.saturating_sub(pair.last_handled_tick) >= day_window {
            pair.incidents_today = 0;
        }
        pair.last_handled_tick = now;
        pair.incidents_today = pair.incidents_today.saturating_add(1);

        let treaties = ledger.treaties(ev.intruder_faction, ev.owner_faction);
        let same_root = registry.root_faction(ev.intruder_faction)
            == registry.root_faction(ev.owner_faction);
        let class = is_trespass(treaties, same_root);

        let mut queue_defense = false;
        match class {
            TrespassClassification::Hostile => {
                // At war — record an Attack incident and always dispatch.
                record_incident(
                    &mut ledger,
                    ev.intruder_faction,
                    ev.owner_faction,
                    now,
                    IncidentKind::Attack {
                        aggressor: ev.intruder_faction,
                        victim_count: 1,
                    },
                );
                queue_defense = true;
                // Intra-fed hostile attack auto-expels aggressor.
                if let Some((fid, _former)) =
                    crate::simulation::federation::expel_on_intra_fed_hostility(
                        &mut federation_map,
                        &mut ledger,
                        ev.intruder_faction,
                        ev.owner_faction,
                        now,
                    )
                {
                    log.send(crate::ui::activity_log::ActivityLogEvent {
                        tick: now,
                        actor: Entity::PLACEHOLDER,
                        faction_id: ev.intruder_faction,
                        kind: crate::ui::activity_log::ActivityEntryKind::FederationExpelled {
                            federation_id: fid,
                            expelled: ev.intruder_faction,
                            reason: crate::simulation::federation::ExpulsionReason::IntraFedAttack,
                        },
                    });
                }
                // Federation defensive propagation — co-members of the
                // owner declare war on the intruder. Defensive-only.
                let bandwagoners =
                    crate::simulation::federation::propagate_federation_defense_on_war(
                        &federation_map,
                        &mut ledger,
                        ev.intruder_faction,
                        ev.owner_faction,
                        now,
                    );
                if let Some(fid) = federation_map
                    .federation_of(ev.owner_faction)
                    .map(|f| f.id)
                {
                    for _ in &bandwagoners {
                        log.send(crate::ui::activity_log::ActivityLogEvent {
                            tick: now,
                            actor: Entity::PLACEHOLDER,
                            faction_id: ev.owner_faction,
                            kind:
                                crate::ui::activity_log::ActivityEntryKind::FederationDefenseTriggered {
                                    federation_id: fid,
                                    attacker: ev.intruder_faction,
                                },
                        });
                    }
                }
            }
            TrespassClassification::Neutral => {
                // Smart-diplomacy P2 — personality-aware grace.
                // Defensive / martial / unknown: 0 extra; mercantile:
                // +1; high trust (> 40): +1 more. Grace shifts every
                // escalation threshold by the same amount.
                let owner_data = registry.factions.get(&ev.owner_faction);
                let trust = ledger
                    .relation(ev.owner_faction, ev.intruder_faction)
                    .map(|r| r.reputation.trust)
                    .unwrap_or(0);
                let mut grace: i32 = 0;
                if let Some(d) = owner_data {
                    let pers = DiplomaticPersonality::from_culture(
                        &d.culture,
                        d.caps.home.is_mobile(),
                    );
                    grace += pers.trespass_warn_grace as i32;
                }
                if trust > 40 {
                    grace += 1;
                }
                let warn_at = 1 + grace; // warning vs no rep change
                let ignore_at = 2 + grace; // IgnoredWarning + grievance
                let queue_at = 3 + grace; // Defense queue push

                let incidents = pair.incidents_today as i32;
                let warned = incidents > warn_at;
                record_incident(
                    &mut ledger,
                    ev.intruder_faction,
                    ev.owner_faction,
                    now,
                    IncidentKind::Trespass {
                        tile: ev.tile,
                        warned,
                    },
                );
                if incidents >= ignore_at {
                    record_incident(
                        &mut ledger,
                        ev.intruder_faction,
                        ev.owner_faction,
                        now,
                        IncidentKind::IgnoredWarning,
                    );
                    queue_defense = incidents >= queue_at;
                }
            }
            TrespassClassification::Allowed => {
                // Shouldn't reach here — detection filters Allowed out.
            }
        }
        if queue_defense {
            queue.push(
                ev.owner_faction,
                DefenseTarget {
                    tile: ev.tile,
                    intruder: ev.intruder,
                    intruder_faction: ev.intruder_faction,
                    expires_tick: now + DEFENSE_TARGET_TTL,
                },
            );
        }
        // Surface a player-facing log entry. `activity_log_ingest_system`
        // filters by `faction_id == player_faction`, so only the player's
        // factions see it.
        log.send(crate::ui::activity_log::ActivityLogEvent {
            tick: now,
            actor: ev.intruder,
            faction_id: ev.owner_faction,
            kind: crate::ui::activity_log::ActivityEntryKind::TrespassWarning {
                intruder_faction: ev.intruder_faction,
                tile: ev.tile,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::diplomacy::TreatyKind;

    #[test]
    fn same_root_is_allowed() {
        let class = is_trespass(TreatySet::default(), true);
        assert_eq!(class, TrespassClassification::Allowed);
    }

    #[test]
    fn alliance_is_allowed() {
        let mut t = TreatySet::default();
        t.insert(TreatyKind::Alliance);
        let class = is_trespass(t, false);
        assert_eq!(class, TrespassClassification::Allowed);
    }

    #[test]
    fn war_is_hostile() {
        let mut t = TreatySet::default();
        t.insert(TreatyKind::War);
        let class = is_trespass(t, false);
        assert_eq!(class, TrespassClassification::Hostile);
    }

    #[test]
    fn unrelated_neutral_is_trespass() {
        let class = is_trespass(TreatySet::default(), false);
        assert_eq!(class, TrespassClassification::Neutral);
    }

    #[test]
    fn defense_queue_evicts_expired() {
        let mut q = TerritoryDefenseQueue::default();
        let world = bevy::ecs::world::World::new();
        let _ = world; // we don't actually need an Entity instance — use placeholder
        q.push(
            7,
            DefenseTarget {
                tile: (0, 0),
                intruder: bevy::ecs::entity::Entity::from_raw(1),
                intruder_faction: 9,
                expires_tick: 10,
            },
        );
        assert!(q.has_target(7));
        q.evict_expired(100);
        assert!(!q.has_target(7));
    }

    #[test]
    fn defense_queue_caps_at_eight() {
        let mut q = TerritoryDefenseQueue::default();
        for i in 0..(DEFENSE_QUEUE_CAP as i32 + 4) {
            q.push(
                1,
                DefenseTarget {
                    tile: (i, 0),
                    intruder: bevy::ecs::entity::Entity::from_raw(1),
                    intruder_faction: 2,
                    expires_tick: 1000,
                },
            );
        }
        assert_eq!(q.by_faction.get(&1).unwrap().len(), DEFENSE_QUEUE_CAP);
    }
}
