//! Animal Husbandry v2.1 — Carts (draftwork haul half).
//!
//! v2.0 shipped the plow half of draftwork (`draftwork.rs`). This module
//! adds the cart half: a composable draft-animal vehicle that ferries bulk
//! construction material from faction storage into blueprints far faster
//! than hand-carry.
//!
//! ## Composable parts
//!
//! A cart is built from a **frame** + two **wheels**, each an independently
//! craftable catalog resource (`cart_frame_{small,medium}`,
//! `cart_wheel_{wood,ironrim}` — recipes 17/18 and 15/16 in `crafting.rs`).
//! [`derive_cart_stats`] composes capacity from the frame size and wheel
//! material:
//!
//! - **Handcart** (small frame) — base 50 kg.
//! - **OxCart** (medium frame) — base 200 kg.
//! - Wooden wheels carry a −10% drag penalty; iron-rimmed wheels don't.
//!
//! ## Assembly
//!
//! [`cart_assembly_system`] (Economy, daily) gives every settled faction
//! with the husbandry tech, a `HitchingPost`, and no cart yet exactly one
//! cart. It **prefers** consuming a pre-crafted `frame + 2 wheels` from
//! storage (so a quality cart built by a Crafter is honoured), and falls
//! back to raw timber (a cartwright building straight from logs) so the
//! autonomous economy reliably produces a cart without a parts-demand
//! signal. The fresh cart parks at the hitching post.
//!
//! ## Haul pipeline (mirrors the v2.0 plow `JobBoard` pattern)
//!
//! 1. **Dispatcher (`htn_cart_haul_dispatch_system`, ParallelB).** For each
//!    `JobClaim::Haul` holder whose `JobProgress::Haul` posting still needs
//!    `>= CART_HAUL_MIN_REMAINING` units, picks a parked cart + a trained
//!    Cattle / Horse, hitches them (inserts `AnimalWorkClaim`), and routes
//!    the worker either to the source storage tile (load phase, cart empty)
//!    or to the blueprint (deliver phase, cart loaded). Re-fires per phase.
//! 2. **Executor (`cart_haul_task_system`, Sequential).** Load phase
//!    transfers up to the cart's capacity (capped at the blueprint's
//!    remaining need) from storage into `CartInventory`. Deliver phase
//!    deposits the load into the blueprint's slots, credits the `Haul`
//!    posting via `record_progress_filtered`, then — once the posting is
//!    complete — releases the `AnimalWorkClaim`, re-parks the cart, and
//!    drops the worker's `JobClaim`.
//! 3. **`cart_follow_system` (Sequential, after movement).** Snaps a
//!    hitched cart's `Transform` to its hauler so the cart visibly trails
//!    the worker.

use bevy::prelude::*;

use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::animals::{
    AnimalUse, AnimalWorkClaim, DomesticAnimal, DomesticSpecies, Tamed,
};
use crate::simulation::construction::Blueprint;
use crate::simulation::draftwork::{release_animal_work_claim, TRAINING_THRESHOLD_DRAFT};
use crate::simulation::faction::{FactionMember, FactionRegistry, StorageTileMap};
use crate::simulation::goals::AgentGoal;
use crate::simulation::husbandry::HitchingPost;
use crate::simulation::items::GroundItem;
use crate::simulation::jobs::{
    record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind, JobProgress,
};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::{ANIMAL_HUSBANDRY, BRONZE_CASTING, OX_CART};
use crate::simulation::typed_task::{ActionQueue, Task, UNEMPLOYED_TASK_KIND};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, world_to_tile, TILE_SIZE};

// ── tunables ────────────────────────────────────────────────────────────────

/// Handcart base cargo capacity (grams). Small frame.
pub const HANDCART_BASE_CAPACITY_G: u32 = 50_000;
/// OxCart base cargo capacity (grams). Medium frame.
pub const OXCART_BASE_CAPACITY_G: u32 = 200_000;

/// Effective-capacity multiplier for a cart on plain wooden wheels — they
/// drag, so a wooden-wheel cart carries 10% less than its iron-rimmed twin.
pub const WOOD_WHEEL_DRAG_NUMER: u32 = 9;
pub const WOOD_WHEEL_DRAG_DENOM: u32 = 10;

/// Cart durability at assembly. Decremented once per completed haul; repair
/// is a v2.1+ follow-up (durability is currently informational — a cart
/// never breaks down).
pub const HANDCART_DURABILITY: u16 = 400;
pub const OXCART_DURABILITY: u16 = 600;

/// Minimum un-delivered quantity on a `JobProgress::Haul` posting before a
/// cart is worth hitching. Below this the per-trip hitch overhead isn't
/// amortised and the worker hand-carries instead.
pub const CART_HAUL_MIN_REMAINING: u32 = 12;

/// Work ticks accumulated at the storage tile / blueprint before the load /
/// deliver phase resolves.
pub const CART_PHASE_WORK_TICKS: u32 = 20;

/// TTL backstop on the cart `AnimalWorkClaim`. The executor explicit-releases
/// on completion; this only matters if the worker dies mid-haul.
pub const CART_CLAIM_TTL_TICKS: u32 = (TICKS_PER_DAY as u32).saturating_mul(2);

// ── components ──────────────────────────────────────────────────────────────

/// Cart size class, derived from the frame the cart was assembled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CartSize {
    Handcart,
    OxCart,
}

impl CartSize {
    /// Pick the size class for a frame resource id.
    pub fn from_frame(frame: ResourceId) -> CartSize {
        if frame == core_ids::cart_frame_medium() {
            CartSize::OxCart
        } else {
            CartSize::Handcart
        }
    }

    pub fn base_capacity_g(self) -> u32 {
        match self {
            CartSize::Handcart => HANDCART_BASE_CAPACITY_G,
            CartSize::OxCart => OXCART_BASE_CAPACITY_G,
        }
    }

    pub fn base_durability(self) -> u16 {
        match self {
            CartSize::Handcart => HANDCART_DURABILITY,
            CartSize::OxCart => OXCART_DURABILITY,
        }
    }
}

/// A draft-animal cart. Spawned by [`cart_assembly_system`]; consumed by the
/// haul pipeline. `hitched_to` / `hauler` are `Some` only while a
/// `Task::CartHaul` is in flight.
#[derive(Component, Clone, Debug)]
pub struct Cart {
    pub size: CartSize,
    /// Frame resource the cart was assembled from (drives `size`).
    pub frame: ResourceId,
    /// Wheel resource (both wheels share a kind in v2.1).
    pub wheels: ResourceId,
    /// Effective cargo capacity in grams (frame base × wheel drag).
    pub capacity_g: u32,
    pub durability: u16,
    pub owner_faction: u32,
    /// Animal currently pulling the cart.
    pub hitched_to: Option<Entity>,
    /// Worker currently driving the cart.
    pub hauler: Option<Entity>,
    /// `HitchingPost` the cart is parked at while idle.
    pub parked_at: Option<Entity>,
}

/// Cargo currently loaded on a cart. One stack per resource kind.
#[derive(Component, Clone, Debug, Default)]
pub struct CartInventory {
    pub items: Vec<(ResourceId, u32)>,
}

impl CartInventory {
    pub fn total_qty(&self) -> u32 {
        self.items.iter().map(|(_, q)| *q).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.items.iter().all(|(_, q)| *q == 0)
    }

    pub fn add(&mut self, rid: ResourceId, qty: u32) {
        if qty == 0 {
            return;
        }
        if let Some(slot) = self.items.iter_mut().find(|(r, _)| *r == rid) {
            slot.1 = slot.1.saturating_add(qty);
        } else {
            self.items.push((rid, qty));
        }
    }

    pub fn qty_of(&self, rid: ResourceId) -> u32 {
        self.items
            .iter()
            .find(|(r, _)| *r == rid)
            .map(|(_, q)| *q)
            .unwrap_or(0)
    }

    /// Remove up to `qty` of `rid`; returns the amount actually removed.
    pub fn take(&mut self, rid: ResourceId, qty: u32) -> u32 {
        if let Some(slot) = self.items.iter_mut().find(|(r, _)| *r == rid) {
            let taken = qty.min(slot.1);
            slot.1 -= taken;
            taken
        } else {
            0
        }
    }
}

/// Render marker — `entity_sprites::spawn_cart_sprites` attaches the child
/// sprite once and stamps this so it isn't re-attached.
#[derive(Component, Clone, Copy, Debug)]
pub struct CartVisual;

// ── stats derivation ────────────────────────────────────────────────────────

/// Compose effective cart stats from a frame + wheel resource pair.
/// Returns `(size, capacity_g, durability)`.
pub fn derive_cart_stats(frame: ResourceId, wheels: ResourceId) -> (CartSize, u32, u16) {
    let size = CartSize::from_frame(frame);
    let base = size.base_capacity_g();
    let capacity = if wheels == core_ids::cart_wheel_ironrim() {
        base
    } else {
        base.saturating_mul(WOOD_WHEEL_DRAG_NUMER) / WOOD_WHEEL_DRAG_DENOM
    };
    (size, capacity, size.base_durability())
}

/// Per-unit weight of a resource from the catalog (grams). Defaults to a
/// conservative 1 kg when the catalog has no entry.
fn unit_weight_g(rid: ResourceId) -> u32 {
    core_ids::catalog()
        .get(rid)
        .map(|d| d.weight_g)
        .filter(|w| *w > 0)
        .unwrap_or(1000)
}

/// How many units of `rid` a cart with `capacity_g` can carry.
pub fn capacity_units(capacity_g: u32, rid: ResourceId) -> u32 {
    (capacity_g / unit_weight_g(rid)).max(1)
}

// ── assembly ────────────────────────────────────────────────────────────────

/// Total available stock of `rid` across a faction's storage tiles.
fn faction_storage_stock(
    faction_id: u32,
    rid: ResourceId,
    storage_tile_map: &StorageTileMap,
    spatial: &SpatialIndex,
    ground_items: &Query<&mut GroundItem>,
) -> u32 {
    let Some(tiles) = storage_tile_map.by_faction.get(&faction_id) else {
        return 0;
    };
    let mut total = 0u32;
    for &(tx, ty) in tiles {
        for &gi_e in spatial.get(tx, ty) {
            if let Ok(gi) = ground_items.get(gi_e) {
                if gi.item.resource_id == rid {
                    total = total.saturating_add(gi.qty);
                }
            }
        }
    }
    total
}

/// Consume `qty` of `rid` from a faction's storage tiles. Returns the amount
/// actually consumed (caller pre-checks stock for an all-or-nothing buy).
fn consume_faction_storage(
    commands: &mut Commands,
    faction_id: u32,
    rid: ResourceId,
    qty: u32,
    storage_tile_map: &StorageTileMap,
    spatial: &SpatialIndex,
    ground_items: &mut Query<&mut GroundItem>,
) -> u32 {
    let Some(tiles) = storage_tile_map.by_faction.get(&faction_id) else {
        return 0;
    };
    let tiles = tiles.clone();
    let mut remaining = qty;
    for (tx, ty) in tiles {
        if remaining == 0 {
            break;
        }
        let entities: Vec<Entity> = spatial.get(tx, ty).to_vec();
        for gi_e in entities {
            if remaining == 0 {
                break;
            }
            if let Ok(mut gi) = ground_items.get_mut(gi_e) {
                if gi.item.resource_id != rid || gi.qty == 0 {
                    continue;
                }
                let take = remaining.min(gi.qty);
                gi.qty -= take;
                remaining -= take;
                if gi.qty == 0 {
                    commands.entity(gi_e).despawn_recursive();
                }
            }
        }
    }
    qty - remaining
}

/// Spawn a parked `Cart` entity at `post` for `faction_id`.
fn spawn_cart(
    commands: &mut Commands,
    faction_id: u32,
    frame: ResourceId,
    wheels: ResourceId,
    post_entity: Entity,
    post_tile: (i32, i32),
) -> Entity {
    let (size, capacity_g, durability) = derive_cart_stats(frame, wheels);
    let wp = tile_to_world(post_tile.0, post_tile.1);
    commands
        .spawn((
            Cart {
                size,
                frame,
                wheels,
                capacity_g,
                durability,
                owner_faction: faction_id,
                hitched_to: None,
                hauler: None,
                parked_at: Some(post_entity),
            },
            CartInventory::default(),
            Transform::from_xyz(wp.x, wp.y, 0.25),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ))
        .id()
}

/// Economy daily system: gives every qualifying settled faction one cart.
///
/// Qualification: faction has `ANIMAL_HUSBANDRY` tech, owns at least one free
/// `HitchingPost`, and has no `Cart` yet. The assembly prefers a pre-crafted
/// `frame + 2 wheels` in storage; failing that it falls back to raw timber
/// + tools so the autonomous economy reliably produces a cart.
pub fn cart_assembly_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    storage_tile_map: Res<StorageTileMap>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut posts: Query<(Entity, &mut HitchingPost)>,
    carts: Query<&Cart>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }

    let mut cart_factions: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for c in carts.iter() {
        cart_factions.insert(c.owner_faction);
    }

    let wood = core_ids::wood();
    let tools = core_ids::tools();

    for (faction_id, faction) in registry.factions.iter() {
        let faction_id = *faction_id;
        if cart_factions.contains(&faction_id) {
            continue;
        }
        if faction.member_count == 0 || !faction.techs.has(ANIMAL_HUSBANDRY) {
            continue;
        }
        // Find a free hitching post owned by this faction.
        let Some((post_entity, post_tile)) = posts.iter().find_map(|(e, p)| {
            if p.faction_id == faction_id && p.parked_cart.is_none() {
                Some((e, p.tile))
            } else {
                None
            }
        }) else {
            continue;
        };

        let has_ox_cart = faction.techs.has(OX_CART);
        let frame = if has_ox_cart {
            core_ids::cart_frame_medium()
        } else {
            core_ids::cart_frame_small()
        };
        let iron_wheels = faction.techs.has(BRONZE_CASTING)
            && faction_storage_stock(
                faction_id,
                core_ids::iron(),
                &storage_tile_map,
                &spatial,
                &ground_items,
            ) >= 2;
        let wheels = if iron_wheels {
            core_ids::cart_wheel_ironrim()
        } else {
            core_ids::cart_wheel_wood()
        };

        // Path A: pre-crafted parts in storage (1 frame + 2 wheels).
        let have_frame = faction_storage_stock(
            faction_id,
            frame,
            &storage_tile_map,
            &spatial,
            &ground_items,
        ) >= 1;
        let have_wheels = faction_storage_stock(
            faction_id,
            wheels,
            &storage_tile_map,
            &spatial,
            &ground_items,
        ) >= 2;
        let assembled = if have_frame && have_wheels {
            consume_faction_storage(
                &mut commands,
                faction_id,
                frame,
                1,
                &storage_tile_map,
                &spatial,
                &mut ground_items,
            );
            consume_faction_storage(
                &mut commands,
                faction_id,
                wheels,
                2,
                &storage_tile_map,
                &spatial,
                &mut ground_items,
            );
            true
        } else {
            // Path B: raw-timber cartwright fallback.
            let (wood_cost, tool_cost) = if has_ox_cart { (30, 4) } else { (15, 2) };
            let wood_stock =
                faction_storage_stock(faction_id, wood, &storage_tile_map, &spatial, &ground_items);
            let tool_stock = faction_storage_stock(
                faction_id,
                tools,
                &storage_tile_map,
                &spatial,
                &ground_items,
            );
            if wood_stock >= wood_cost && tool_stock >= tool_cost {
                consume_faction_storage(
                    &mut commands,
                    faction_id,
                    wood,
                    wood_cost,
                    &storage_tile_map,
                    &spatial,
                    &mut ground_items,
                );
                consume_faction_storage(
                    &mut commands,
                    faction_id,
                    tools,
                    tool_cost,
                    &storage_tile_map,
                    &spatial,
                    &mut ground_items,
                );
                true
            } else {
                false
            }
        };

        if !assembled {
            continue;
        }

        let cart = spawn_cart(
            &mut commands,
            faction_id,
            frame,
            wheels,
            post_entity,
            post_tile,
        );
        if let Ok((_, mut post)) = posts.get_mut(post_entity) {
            post.parked_cart = Some(cart);
        }
        cart_factions.insert(faction_id);
    }
}

// ── draft-animal pick ───────────────────────────────────────────────────────

/// Find one trained Cattle / Horse owned by `faction_id` not already claimed
/// (`Without<AnimalWorkClaim>`) and not in `taken` (claimed earlier this pass).
fn pick_idle_draft_animal(
    faction_id: u32,
    animals_q: &Query<(Entity, &DomesticAnimal, &Tamed), Without<AnimalWorkClaim>>,
    taken: &ahash::AHashSet<Entity>,
) -> Option<Entity> {
    for (e, da, tamed) in animals_q.iter() {
        if tamed.owner_faction != faction_id || taken.contains(&e) {
            continue;
        }
        if da.training < TRAINING_THRESHOLD_DRAFT {
            continue;
        }
        if matches!(da.species, DomesticSpecies::Cattle | DomesticSpecies::Horse) {
            return Some(e);
        }
    }
    None
}

/// Nearest tile in `tiles` to `from` by chebyshev distance.
fn nearest_tile(from: (i32, i32), tiles: &[(i32, i32)]) -> Option<(i32, i32)> {
    tiles
        .iter()
        .copied()
        .min_by_key(|&(x, y)| (x - from.0).abs().max((y - from.1).abs()))
}

// ── haul dispatcher ─────────────────────────────────────────────────────────

/// ParallelB dispatcher. Routes `JobClaim::Haul` holders through a cart when
/// one is available and the haul is bulky enough to amortise the hitch.
#[allow(clippy::too_many_arguments)]
pub fn htn_cart_haul_dispatch_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    board: Res<JobBoard>,
    storage_tile_map: Res<StorageTileMap>,
    mut carts_q: Query<(Entity, &mut Cart, &CartInventory)>,
    animals_q: Query<(Entity, &DomesticAnimal, &Tamed), Without<AnimalWorkClaim>>,
    mut posts_q: Query<(Entity, &mut HitchingPost)>,
    bp_q: Query<&Blueprint>,
    mut workers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
            &BucketSlot,
            &JobClaim,
        ),
        (With<Person>, Without<Drafted>),
    >,
) {
    let now = clock.tick as u32;
    // Animals claimed earlier in this same pass (the `AnimalWorkClaim` insert
    // is deferred, so `Without<AnimalWorkClaim>` can't see it yet).
    let mut claimed_this_pass: ahash::AHashSet<Entity> = ahash::AHashSet::default();

    for (worker, mut ai, mut aq, goal, fm, tr, lod, slot, claim) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if !matches!(claim.kind, JobKind::Haul) || !matches!(*goal, AgentGoal::Haul) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        let Some(posting) = board.get(claim.job_id) else {
            continue;
        };
        let (blueprint, resource_id, delivered, target) = match posting.progress {
            JobProgress::Haul {
                blueprint,
                resource_id,
                delivered,
                target,
                ..
            } => (blueprint, resource_id, delivered, target),
            _ => continue,
        };
        if target.saturating_sub(delivered) < CART_HAUL_MIN_REMAINING {
            continue;
        }

        // Resume this worker's in-flight cart, or claim a fresh one.
        let resumed = carts_q
            .iter()
            .find(|(_, c, _)| c.hauler == Some(worker))
            .map(|(e, c, inv)| (e, c.hitched_to, !inv.is_empty()));

        let (cart_e, animal_e, cart_loaded) = if let Some((cart_e, hitched, loaded)) = resumed {
            let Some(animal_e) = hitched else {
                continue;
            };
            (cart_e, animal_e, loaded)
        } else {
            // Fresh hitch: need a parked cart + a trained animal.
            let cart_pick = carts_q.iter().find_map(|(e, c, _)| {
                if c.owner_faction == fm.faction_id
                    && c.hauler.is_none()
                    && c.hitched_to.is_none()
                    && c.durability > 0
                {
                    Some(e)
                } else {
                    None
                }
            });
            let Some(cart_e) = cart_pick else {
                continue;
            };
            let Some(animal_e) =
                pick_idle_draft_animal(fm.faction_id, &animals_q, &claimed_this_pass)
            else {
                continue;
            };
            // Hitch: mark the cart in-use, free its post, claim the animal.
            let mut freed_post: Option<Entity> = None;
            if let Ok((_, mut cart, _)) = carts_q.get_mut(cart_e) {
                cart.hauler = Some(worker);
                cart.hitched_to = Some(animal_e);
                freed_post = cart.parked_at.take();
            }
            if let Some(post_e) = freed_post {
                if let Ok((_, mut post)) = posts_q.get_mut(post_e) {
                    if post.parked_cart == Some(cart_e) {
                        post.parked_cart = None;
                    }
                }
            }
            commands.entity(animal_e).insert(AnimalWorkClaim {
                worker,
                use_kind: AnimalUse::Cart,
                expires_tick: now.saturating_add(CART_CLAIM_TTL_TICKS),
            });
            claimed_this_pass.insert(animal_e);
            (cart_e, animal_e, false)
        };

        let worker_tile = world_to_tile(tr.translation.truncate());
        let cur_chunk = ChunkCoord(
            worker_tile.0.div_euclid(CHUNK_SIZE as i32),
            worker_tile.1.div_euclid(CHUNK_SIZE as i32),
        );

        // Phase: empty cart → load at storage; loaded cart → deliver at bp.
        let target_tile = if cart_loaded {
            let Ok(bp) = bp_q.get(blueprint) else {
                continue;
            };
            bp.work_stand.unwrap_or(bp.tile)
        } else {
            let Some(src) = storage_tile_map
                .by_faction
                .get(&fm.faction_id)
                .and_then(|tiles| nearest_tile(worker_tile, tiles))
            else {
                continue;
            };
            src
        };

        let routed = assign_task_with_routing(
            &mut ai,
            worker_tile,
            cur_chunk,
            target_tile,
            TaskKind::CartHaul,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !routed {
            continue;
        }
        let _ = aq.dispatch(Task::CartHaul {
            cart: cart_e,
            animal: animal_e,
            blueprint,
            resource_id,
        });
    }
}

// ── haul executor ───────────────────────────────────────────────────────────

/// Sum of a blueprint's unmet deposit slots for `rid`.
fn blueprint_remaining_need(bp: &Blueprint, rid: ResourceId) -> u32 {
    let mut total = 0u32;
    for i in 0..bp.deposit_count as usize {
        if bp.deposits[i].resource_id == rid {
            total = total.saturating_add(
                bp.deposits[i]
                    .needed
                    .saturating_sub(bp.deposits[i].deposited) as u32,
            );
        }
    }
    total
}

/// Re-park a cart at the nearest free hitching post of its owner faction and
/// clear `hauler` / `hitched_to`.
fn repark_cart(
    carts_q: &mut Query<(&mut Cart, &mut CartInventory)>,
    posts_q: &mut Query<(Entity, &mut HitchingPost)>,
    cart_e: Entity,
) {
    let faction = carts_q
        .get(cart_e)
        .map(|(c, _)| c.owner_faction)
        .unwrap_or(u32::MAX);
    let post_e = posts_q
        .iter()
        .find(|(_, p)| p.faction_id == faction && p.parked_cart.is_none())
        .map(|(e, _)| e);
    if let Ok((mut cart, _)) = carts_q.get_mut(cart_e) {
        cart.hauler = None;
        cart.hitched_to = None;
        cart.parked_at = post_e;
    }
    if let Some(pe) = post_e {
        if let Ok((_, mut post)) = posts_q.get_mut(pe) {
            post.parked_cart = Some(cart_e);
        }
    }
}

/// Sequential executor for `Task::CartHaul`.
#[allow(clippy::too_many_arguments)]
pub fn cart_haul_task_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut bp_q: Query<&mut Blueprint>,
    mut carts_q: Query<(&mut Cart, &mut CartInventory)>,
    mut posts_q: Query<(Entity, &mut HitchingPost)>,
    mut workers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &BucketSlot,
            &LodLevel,
            &JobClaim,
        ),
        With<Person>,
    >,
) {
    for (worker, mut ai, mut aq, slot, lod, claim) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::CartHaul as u16 {
            continue;
        }
        let Some((cart_e, animal_e, blueprint, rid)) = aq.current.as_cart_haul() else {
            aq.cancel_chain(&mut ai);
            continue;
        };
        if ai.state != AiState::Working {
            continue;
        }
        if !matches!(claim.kind, JobKind::Haul) {
            release_animal_work_claim(&mut commands, animal_e);
            repark_cart(&mut carts_q, &mut posts_q, cart_e);
            aq.cancel_chain(&mut ai);
            continue;
        }
        if (ai.work_progress as u32) < CART_PHASE_WORK_TICKS {
            continue;
        }

        // Snapshot cart capacity + load state; abort if the cart vanished.
        let Ok((cart_capacity, cart_loaded)) = carts_q
            .get(cart_e)
            .map(|(c, inv)| (c.capacity_g, !inv.is_empty()))
        else {
            release_animal_work_claim(&mut commands, animal_e);
            commands.entity(worker).remove::<JobClaim>();
            aq.cancel_chain(&mut ai);
            continue;
        };

        if !cart_loaded {
            // ── Load phase ──────────────────────────────────────────────
            let need = bp_q
                .get(blueprint)
                .map(|bp| blueprint_remaining_need(&bp, rid))
                .unwrap_or(0);
            if need == 0 {
                release_animal_work_claim(&mut commands, animal_e);
                repark_cart(&mut carts_q, &mut posts_q, cart_e);
                commands.entity(worker).remove::<JobClaim>();
                aq.finish_task(&mut ai);
                continue;
            }
            let want = need.min(capacity_units(cart_capacity, rid));
            let (sx, sy) = ai.dest_tile;
            let entities: Vec<Entity> = spatial.get(sx, sy).to_vec();
            let mut loaded = 0u32;
            for gi_e in entities {
                if loaded >= want {
                    break;
                }
                if let Ok(mut gi) = ground_items.get_mut(gi_e) {
                    if gi.item.resource_id != rid || gi.qty == 0 {
                        continue;
                    }
                    let take = (want - loaded).min(gi.qty);
                    gi.qty -= take;
                    loaded += take;
                    if gi.qty == 0 {
                        commands.entity(gi_e).despawn_recursive();
                    }
                }
            }
            if loaded == 0 {
                // Storage empty of `rid` — abort the cart haul; the worker
                // re-plans (a hand-haul method may pick the posting up once
                // storage refills).
                release_animal_work_claim(&mut commands, animal_e);
                repark_cart(&mut carts_q, &mut posts_q, cart_e);
                commands.entity(worker).remove::<JobClaim>();
                aq.cancel_chain(&mut ai);
                continue;
            }
            if let Ok((_, mut inv)) = carts_q.get_mut(cart_e) {
                inv.add(rid, loaded);
            }
            // Drop to Idle — the dispatcher routes the deliver leg next pass.
            aq.finish_task(&mut ai);
        } else {
            // ── Deliver phase ───────────────────────────────────────────
            let carried = carts_q
                .get(cart_e)
                .map(|(_, inv)| inv.qty_of(rid))
                .unwrap_or(0);
            let mut deposited = 0u32;
            if let Ok(mut bp) = bp_q.get_mut(blueprint) {
                let mut remaining = carried;
                for i in 0..bp.deposit_count as usize {
                    if remaining == 0 {
                        break;
                    }
                    if bp.deposits[i].resource_id != rid {
                        continue;
                    }
                    let still = bp.deposits[i]
                        .needed
                        .saturating_sub(bp.deposits[i].deposited)
                        as u32;
                    let take = still.min(remaining).min(u8::MAX as u32);
                    bp.deposits[i].deposited = bp.deposits[i].deposited.saturating_add(take as u8);
                    remaining -= take;
                    deposited += take;
                }
            }
            let residual = {
                let mut res = 0u32;
                if let Ok((_, mut inv)) = carts_q.get_mut(cart_e) {
                    inv.take(rid, carried);
                    res = carried.saturating_sub(deposited);
                }
                res
            };
            if residual > 0 {
                let (sx, sy) = ai.dest_tile;
                crate::simulation::items::spawn_or_merge_ground_item(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    sx,
                    sy,
                    rid,
                    residual,
                );
            }
            if deposited > 0 {
                record_progress_filtered(
                    &mut commands,
                    &mut board,
                    &mut completed_events,
                    claim,
                    JobKind::Haul,
                    Some(rid),
                    deposited,
                );
            }
            if let Ok((mut cart, _)) = carts_q.get_mut(cart_e) {
                cart.durability = cart.durability.saturating_sub(1);
            }
            let posting_done = board
                .get(claim.job_id)
                .map(|p| p.progress.is_complete())
                .unwrap_or(true);
            if posting_done {
                release_animal_work_claim(&mut commands, animal_e);
                repark_cart(&mut carts_q, &mut posts_q, cart_e);
                commands.entity(worker).remove::<JobClaim>();
                aq.finish_task(&mut ai);
            } else {
                // More to haul — keep the cart hitched, drop to Idle so the
                // dispatcher routes another load leg.
                aq.finish_task(&mut ai);
            }
        }
    }
}

// ── cart follow ─────────────────────────────────────────────────────────────

/// Sequential (after movement): snap a hitched cart's `Transform` to its
/// hauler so the cart visibly trails the worker.
pub fn cart_follow_system(
    workers: Query<&Transform, (With<Person>, Without<Cart>)>,
    mut carts: Query<(&Cart, &mut Transform)>,
) {
    for (cart, mut tf) in carts.iter_mut() {
        let Some(hauler) = cart.hauler else {
            continue;
        };
        if let Ok(htf) = workers.get(hauler) {
            tf.translation.x = htf.translation.x - TILE_SIZE * 0.6;
            tf.translation.y = htf.translation.y - TILE_SIZE * 0.4;
            tf.translation.z = 0.25;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handcart_smaller_than_oxcart() {
        let frame_s = core_ids::cart_frame_small();
        let frame_m = core_ids::cart_frame_medium();
        let wheel = core_ids::cart_wheel_wood();
        let (_, cap_s, _) = derive_cart_stats(frame_s, wheel);
        let (_, cap_m, _) = derive_cart_stats(frame_m, wheel);
        assert!(cap_m > cap_s * 3, "OxCart should be >3× Handcart capacity");
    }

    #[test]
    fn iron_rim_wheel_outperforms_wood_wheel() {
        let frame = core_ids::cart_frame_medium();
        let (_, cap_wood, _) = derive_cart_stats(frame, core_ids::cart_wheel_wood());
        let (_, cap_iron, _) = derive_cart_stats(frame, core_ids::cart_wheel_ironrim());
        assert!(
            cap_iron > cap_wood,
            "iron-rimmed wheels remove the wooden-wheel drag penalty"
        );
    }

    #[test]
    fn cart_size_classifies_from_frame() {
        assert_eq!(
            CartSize::from_frame(core_ids::cart_frame_medium()),
            CartSize::OxCart
        );
        assert_eq!(
            CartSize::from_frame(core_ids::cart_frame_small()),
            CartSize::Handcart
        );
    }

    #[test]
    fn capacity_units_is_positive() {
        let (_, cap, _) =
            derive_cart_stats(core_ids::cart_frame_medium(), core_ids::cart_wheel_wood());
        assert!(capacity_units(cap, core_ids::wood()) >= 1);
        assert!(capacity_units(cap, core_ids::stone()) >= 1);
    }

    #[test]
    fn cart_inventory_add_take_roundtrip() {
        let mut inv = CartInventory::default();
        assert!(inv.is_empty());
        inv.add(core_ids::stone(), 40);
        assert_eq!(inv.total_qty(), 40);
        assert!(!inv.is_empty());
        let took = inv.take(core_ids::stone(), 25);
        assert_eq!(took, 25);
        assert_eq!(inv.qty_of(core_ids::stone()), 15);
    }
}
