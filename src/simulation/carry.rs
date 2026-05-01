//! Hand-carry (Carrier) component and helpers.
//!
//! A `Carrier` represents what an agent is physically holding in their hands. Hand contents
//! are SEPARATE from `EconomicAgent.inventory` (which is the agent's personal belongings,
//! ~5 kg of food, tools, seeds, etc).
//!
//! Hauling and gathering route loads through hands. Personal pickups (a snack, a tool to
//! wield) go to inventory. Tasks like construct/craft/dig require free hand(s); see
//! `tasks::task_requires_free_hands`.

use crate::economy::goods::Bulk;
use crate::economy::item::Item;
use bevy::prelude::*;

/// Per-hand load ceiling, in grams. Two hands → up to ~50 kg combined.
pub const HUMAN_HAND_CAP_G: u32 = 25_000;

/// Per-hand quantity cap. A worker fills a hand with at most this many units of one
/// item before they need to head back and deposit. Drives the gather→deposit cycle.
pub const HAND_QTY_CAP: u32 = 3;

/// One stack held in one or both hands.
#[derive(Clone, Copy, Debug)]
pub struct HeldStack {
    pub item: Item,
    pub qty: u32,
    /// True when this stack occupies both hands (Bulk::TwoHand goods).
    pub two_handed: bool,
}

impl HeldStack {
    pub fn weight_g(&self) -> u32 {
        self.item.stack_weight_g(self.qty)
    }
}

/// Two hand slots. Empty by default.
#[derive(Component, Clone, Copy, Default, Debug)]
pub struct Carrier {
    pub left: Option<HeldStack>,
    pub right: Option<HeldStack>,
}

impl Carrier {
    pub fn is_empty(&self) -> bool {
        self.left.is_none() && self.right.is_none()
    }

    /// Number of hands free (0/1/2). A two-handed stack occupies both hands but is stored
    /// in the left slot only, so we still report 0 free hands when present.
    pub fn free_hands(&self) -> u8 {
        match (self.left, self.right) {
            (None, None) => 2,
            (Some(s), _) if s.two_handed => 0,
            (None, Some(_)) | (Some(_), None) => 1,
            (Some(_), Some(_)) => 0,
        }
    }

    pub fn has_two_handed_load(&self) -> bool {
        self.left.map(|s| s.two_handed).unwrap_or(false)
    }

    pub fn total_weight_g(&self) -> u32 {
        let l = self.left.map(|s| s.weight_g()).unwrap_or(0);
        let r = self.right.map(|s| s.weight_g()).unwrap_or(0);
        l.saturating_add(r)
    }

    /// Total quantity of `item` currently held across both hands.
    pub fn quantity_of(&self, item: Item) -> u32 {
        let mut q = 0u32;
        if let Some(s) = self.left {
            if s.item == item {
                q = q.saturating_add(s.qty);
            }
        }
        if let Some(s) = self.right {
            if s.item == item {
                q = q.saturating_add(s.qty);
            }
        }
        q
    }

    /// Total quantity of `good` (any material/quality) across both hands.
    pub fn quantity_of_good(&self, good: crate::economy::goods::Good) -> u32 {
        let mut q = 0u32;
        if let Some(s) = self.left {
            if s.item.good == good {
                q = q.saturating_add(s.qty);
            }
        }
        if let Some(s) = self.right {
            if s.item.good == good {
                q = q.saturating_add(s.qty);
            }
        }
        q
    }

    /// True if at least one unit of `good` could be picked up given current hand state
    /// (does not consider weight cap exhaustively — used as a coarse "room left" check).
    pub fn can_accept(&self, good: crate::economy::goods::Good) -> bool {
        match good.bulk() {
            Bulk::TwoHand => self.left.is_none() && self.right.is_none(),
            Bulk::OneHand => !self.has_two_handed_load() && self.free_hands() > 0,
            Bulk::Small => {
                if self.has_two_handed_load() {
                    return false;
                }
                if self.free_hands() > 0 {
                    return true;
                }
                let item = crate::economy::item::Item::new_commodity(good);
                let unit_w = item.unit_weight_g().max(1);
                for slot in [self.left, self.right].iter().flatten() {
                    if !slot.two_handed && slot.item == item {
                        let used = slot.weight_g();
                        if HUMAN_HAND_CAP_G.saturating_sub(used) >= unit_w {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    /// Try to pick up `qty` of `item` into hands. Returns leftover that did not fit.
    /// Respects bulk class (two-handed needs both hands free) and per-hand weight cap.
    pub fn try_pick_up(&mut self, item: Item, qty: u32) -> u32 {
        if qty == 0 {
            return 0;
        }
        let unit_w = item.unit_weight_g().max(1);
        let bulk = item.good.bulk();

        match bulk {
            Bulk::TwoHand => {
                if self.left.is_some() || self.right.is_some() {
                    return qty;
                }
                let cap = HUMAN_HAND_CAP_G.saturating_mul(2);
                let by_weight = cap / unit_w;
                let take = qty.min(by_weight).min(HAND_QTY_CAP);
                if take == 0 {
                    return qty;
                }
                self.left = Some(HeldStack {
                    item,
                    qty: take,
                    two_handed: true,
                });
                qty - take
            }
            Bulk::OneHand => {
                if self.has_two_handed_load() {
                    return qty;
                }
                let by_weight = HUMAN_HAND_CAP_G / unit_w;
                if by_weight == 0 {
                    return qty;
                }
                let take = qty.min(by_weight).min(HAND_QTY_CAP);
                let stack = HeldStack {
                    item,
                    qty: take,
                    two_handed: false,
                };
                if self.left.is_none() {
                    self.left = Some(stack);
                    return qty - take;
                }
                if self.right.is_none() {
                    self.right = Some(stack);
                    return qty - take;
                }
                qty
            }
            Bulk::Small => {
                if self.has_two_handed_load() {
                    return qty;
                }
                let mut remaining = qty;
                // Top up matching stack first (left then right).
                for slot in [&mut self.left, &mut self.right] {
                    if let Some(stack) = slot.as_mut() {
                        if stack.item == item && !stack.two_handed {
                            let used = stack.weight_g();
                            let cap_left = HUMAN_HAND_CAP_G.saturating_sub(used);
                            let by_weight = cap_left / unit_w;
                            let qty_room = HAND_QTY_CAP.saturating_sub(stack.qty);
                            let take = remaining.min(by_weight).min(qty_room);
                            if take > 0 {
                                stack.qty = stack.qty.saturating_add(take);
                                remaining -= take;
                                if remaining == 0 {
                                    return 0;
                                }
                            }
                        }
                    }
                }
                // Claim an empty hand.
                for slot in [&mut self.left, &mut self.right] {
                    if slot.is_none() {
                        let by_weight = HUMAN_HAND_CAP_G / unit_w;
                        let take = remaining.min(by_weight).min(HAND_QTY_CAP);
                        if take > 0 {
                            *slot = Some(HeldStack {
                                item,
                                qty: take,
                                two_handed: false,
                            });
                            remaining -= take;
                            if remaining == 0 {
                                return 0;
                            }
                        }
                    }
                }
                remaining
            }
        }
    }

    /// True when the worker has hauled enough that they should head back to
    /// deposit. A two-handed stack at the per-hand cap is enough on its own;
    /// otherwise both hands must be occupied with at least one at the cap.
    pub fn is_at_haul_cap(&self) -> bool {
        if let Some(s) = self.left {
            if s.two_handed {
                return s.qty >= HAND_QTY_CAP;
            }
        }
        let l_full = self.left.map_or(false, |s| s.qty >= HAND_QTY_CAP);
        let r_full = self.right.map_or(false, |s| s.qty >= HAND_QTY_CAP);
        self.left.is_some() && self.right.is_some() && (l_full || r_full)
    }

    /// Remove up to `qty` of `item` from hands. Returns how many were actually removed.
    /// Drains left first, then right; clears empty stacks.
    pub fn remove_item(&mut self, item: Item, qty: u32) -> u32 {
        let mut removed = 0u32;
        let mut want = qty;
        for slot in [&mut self.left, &mut self.right] {
            if want == 0 {
                break;
            }
            if let Some(stack) = slot.as_mut() {
                if stack.item == item {
                    let take = stack.qty.min(want);
                    stack.qty -= take;
                    removed += take;
                    want -= take;
                    if stack.qty == 0 {
                        *slot = None;
                    }
                }
            }
        }
        removed
    }

    pub fn remove_good(&mut self, good: crate::economy::goods::Good, qty: u32) -> u32 {
        let mut removed = 0u32;
        let mut want = qty;
        for slot in [&mut self.left, &mut self.right] {
            if want == 0 {
                break;
            }
            if let Some(stack) = slot.as_mut() {
                if stack.item.good == good {
                    let take = stack.qty.min(want);
                    stack.qty -= take;
                    removed += take;
                    want -= take;
                    if stack.qty == 0 {
                        *slot = None;
                    }
                }
            }
        }
        removed
    }

    /// Clear both hands and return whatever was held.
    pub fn drop_all(&mut self) -> Vec<HeldStack> {
        let mut out = Vec::new();
        if let Some(s) = self.left.take() {
            out.push(s);
        }
        if let Some(s) = self.right.take() {
            out.push(s);
        }
        out
    }

    /// Drop one hand's contents (heaviest first). Used by combat when no free hand is available.
    pub fn drop_one_hand(&mut self) -> Option<HeldStack> {
        // Two-handed loads occupy left only; dropping that frees both hands.
        if let Some(s) = self.left {
            if s.two_handed {
                self.left = None;
                return Some(s);
            }
        }
        let lw = self.left.map(|s| s.weight_g()).unwrap_or(0);
        let rw = self.right.map(|s| s.weight_g()).unwrap_or(0);
        if lw >= rw {
            self.left.take().or_else(|| self.right.take())
        } else {
            self.right.take().or_else(|| self.left.take())
        }
    }
}

/// Drop everything in `carrier` to ground at tile `(tx, ty)` as `GroundItem` entities,
/// merging into existing stacks of the same good.
pub fn drop_carrier_to_ground(
    commands: &mut Commands,
    spatial: &crate::world::spatial::SpatialIndex,
    item_query: &mut Query<&mut crate::simulation::items::GroundItem>,
    carrier: &mut Carrier,
    tx: i32,
    ty: i32,
) {
    let stacks = carrier.drop_all();
    for stack in stacks {
        crate::simulation::items::spawn_or_merge_ground_item(
            commands,
            spatial,
            item_query,
            tx,
            ty,
            stack.item.good,
            stack.qty,
        );
    }
}

/// Enforce hand-occupancy requirements for the agent's current task. Drops hand
/// stacks to ground at the agent's tile when the task needs more free hands than
/// available, or when the task is incompatible with carrying anything (Sleep,
/// Socialize, Reproduce, Eat).
///
/// Runs once per Sequential tick before the work systems, so an agent who arrives
/// at a worksite with their hands full sets down their load and can begin work.
pub fn enforce_hand_state_system(
    mut commands: Commands,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut item_query: Query<&mut crate::simulation::items::GroundItem>,
    mut agents: Query<
        (
            &crate::simulation::person::PersonAI,
            &mut Carrier,
            &Transform,
            &crate::simulation::lod::LodLevel,
        ),
        With<crate::simulation::person::Person>,
    >,
) {
    use crate::simulation::lod::LodLevel;
    use crate::simulation::person::AiState;
    use crate::simulation::tasks::{task_drops_hand_load, task_requires_free_hands};
    use crate::world::terrain::world_to_tile;

    for (ai, mut carrier, transform, lod) in agents.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if carrier.is_empty() {
            continue;
        }

        let drop_all = task_drops_hand_load(ai.task_id);
        let need_free = task_requires_free_hands(ai.task_id);
        let have_free = carrier.free_hands();

        if !drop_all && need_free <= have_free {
            continue;
        }

        let (tx, ty) = world_to_tile(transform.translation.truncate());
        if drop_all {
            drop_carrier_to_ground(
                &mut commands,
                &spatial,
                &mut item_query,
                &mut carrier,
                tx,
                ty,
            );
        } else {
            // Drop one hand at a time until we have enough free, or hands are empty.
            while carrier.free_hands() < need_free {
                if let Some(stack) = carrier.drop_one_hand() {
                    crate::simulation::items::spawn_or_merge_ground_item(
                        &mut commands,
                        &spatial,
                        &mut item_query,
                        tx,
                        ty,
                        stack.item.good,
                        stack.qty,
                    );
                } else {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::goods::Good;
    use crate::economy::item::Item;

    #[test]
    fn small_items_stack_in_one_hand() {
        let mut c = Carrier::default();
        let item = Item::new_commodity(Good::Fruit);
        let leftover = c.try_pick_up(item, 3);
        assert_eq!(leftover, 0);
        assert_eq!(c.quantity_of(item), 3);
        assert_eq!(c.free_hands(), 1);
    }

    #[test]
    fn small_items_capped_at_three_per_hand() {
        let mut c = Carrier::default();
        let item = Item::new_commodity(Good::Fruit);
        let leftover = c.try_pick_up(item, 10);
        assert_eq!(leftover, 4, "qty cap fills both hands at 3 each = 6");
        assert_eq!(c.quantity_of(item), 6);
        assert_eq!(c.free_hands(), 0);
    }

    #[test]
    fn two_handed_log_takes_both_hands() {
        let mut c = Carrier::default();
        let log = Item::new_commodity(Good::Wood);
        let leftover = c.try_pick_up(log, 3);
        assert_eq!(leftover, 0);
        assert_eq!(c.free_hands(), 0);
        assert!(c.has_two_handed_load());
    }

    #[test]
    fn two_handed_capped_at_three() {
        let mut c = Carrier::default();
        let log = Item::new_commodity(Good::Wood);
        let leftover = c.try_pick_up(log, 4);
        assert_eq!(leftover, 1, "two-handed stack caps at HAND_QTY_CAP");
        assert_eq!(c.quantity_of(log), 3);
    }

    #[test]
    fn two_handed_blocked_when_a_hand_occupied() {
        let mut c = Carrier::default();
        let _ = c.try_pick_up(Item::new_commodity(Good::Fruit), 1);
        let log = Item::new_commodity(Good::Wood);
        let leftover = c.try_pick_up(log, 1);
        assert_eq!(
            leftover, 1,
            "two-handed pickup must fail with any hand busy"
        );
    }

    #[test]
    fn over_cap_returns_leftover() {
        // Stone is TwoHand; qty cap (3) binds before the weight cap.
        let mut c = Carrier::default();
        let stone = Item::new_commodity(Good::Stone);
        let leftover = c.try_pick_up(stone, 100);
        assert_eq!(leftover, 97);
        assert_eq!(c.quantity_of(stone), 3);
        assert!(c.total_weight_g() <= HUMAN_HAND_CAP_G * 2);
    }

    #[test]
    fn is_at_haul_cap_triggers_when_two_handed_full() {
        let mut c = Carrier::default();
        let log = Item::new_commodity(Good::Wood);
        let _ = c.try_pick_up(log, 3);
        assert!(c.is_at_haul_cap());
    }

    #[test]
    fn is_at_haul_cap_requires_both_hands_for_one_hand_goods() {
        let mut c = Carrier::default();
        let coal = Item::new_commodity(Good::Coal); // OneHand
        let _ = c.try_pick_up(coal, 3);
        assert!(!c.is_at_haul_cap(), "one filled hand isn't enough");
        let _ = c.try_pick_up(coal, 3);
        assert!(c.is_at_haul_cap(), "both hands at cap → return");
    }

    #[test]
    fn drop_one_hand_prefers_heaviest() {
        let mut c = Carrier::default();
        let _ = c.try_pick_up(Item::new_commodity(Good::Coal), 3); // ~10 kg per hand
        let _ = c.try_pick_up(Item::new_commodity(Good::Skin), 3); // ~4.5 kg per hand
        let dropped = c.drop_one_hand().expect("should drop something");
        assert_eq!(dropped.item.good, Good::Coal);
    }
}
