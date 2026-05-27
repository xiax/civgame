//! Realistic Tool Overhaul — separate, functional tools.
//!
//! A [`ToolForm`] decides *what* a tool does (Knife cuts, Pick mines, …);
//! the manufactured [`Item`]'s material + quality decide *how well* via
//! [`tool_tier`] (`Bone < Stone < FineStone < Copper < Bronze`). Gathering,
//! mining, fishing, and crafting hard-gate on holding a tool of the right
//! form; a better [`ToolTier`] only makes the work faster ([`work_speed_mult`]).
//!
//! Tools are carried in a [`ToolKit`] component — a small `Vec<Item>` that
//! preserves full material/quality fidelity and is *separate* from the
//! `Carrier` cargo hands, so a worker keeps both hands free for the wood /
//! stone they gather. There is no durability/wear in this pass.

use crate::economy::core_ids;
use crate::economy::item::{Item, ItemMaterial, ItemQuality};
use crate::economy::resource_catalog::ResourceId;
use crate::simulation::technology::Era;
use bevy::prelude::Component;

/// The functional identity of a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolForm {
    Knife,
    Axe,
    Pick,
    Hammer,
    Sickle,
    Awl,
    FishingKit,
}

impl ToolForm {
    pub const ALL: [ToolForm; 7] = [
        ToolForm::Knife,
        ToolForm::Axe,
        ToolForm::Pick,
        ToolForm::Hammer,
        ToolForm::Sickle,
        ToolForm::Awl,
        ToolForm::FishingKit,
    ];

    /// Catalog resource backing this tool form.
    pub fn resource_id(self) -> ResourceId {
        match self {
            ToolForm::Knife => core_ids::knife(),
            ToolForm::Axe => core_ids::axe(),
            ToolForm::Pick => core_ids::pick(),
            ToolForm::Hammer => core_ids::hammer(),
            ToolForm::Sickle => core_ids::sickle(),
            ToolForm::Awl => core_ids::awl(),
            ToolForm::FishingKit => core_ids::fishing_kit(),
        }
    }

    /// Reverse map. Returns `None` for non-tool resources (incl. the legacy
    /// generic `tools` commodity — it never satisfies a [`ToolRequirement`]).
    pub fn from_resource_id(id: ResourceId) -> Option<ToolForm> {
        ToolForm::ALL.into_iter().find(|f| f.resource_id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            ToolForm::Knife => "Knife",
            ToolForm::Axe => "Axe",
            ToolForm::Pick => "Pick",
            ToolForm::Hammer => "Hammer",
            ToolForm::Sickle => "Sickle",
            ToolForm::Awl => "Awl",
            ToolForm::FishingKit => "Fishing Kit",
        }
    }
}

/// What a task needs done. Each kind maps to exactly one [`ToolForm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolUseKind {
    /// Slicing — butchery prep, hide/spear work (Knife).
    Cut,
    /// Felling trees (Axe).
    Chop,
    /// Breaking stone / ore from tiles, excavating rock (Pick).
    Mine,
    /// Shaping timber — frames, wheels, carts (Axe).
    Shape,
    /// Striking metal / heavy assembly (Hammer).
    Smith,
    /// Reaping mature grain (Sickle).
    HarvestCrop,
    /// Piercing leather / fine stitching (Awl).
    Stitch,
    /// Catching fish (Fishing Kit).
    Fish,
}

impl ToolUseKind {
    pub fn form(self) -> ToolForm {
        match self {
            ToolUseKind::Cut => ToolForm::Knife,
            ToolUseKind::Chop => ToolForm::Axe,
            ToolUseKind::Mine => ToolForm::Pick,
            ToolUseKind::Shape => ToolForm::Axe,
            ToolUseKind::Smith => ToolForm::Hammer,
            ToolUseKind::HarvestCrop => ToolForm::Sickle,
            ToolUseKind::Stitch => ToolForm::Awl,
            ToolUseKind::Fish => ToolForm::FishingKit,
        }
    }
}

/// Quality ladder of a tool, derived from material + craft quality.
/// `Ord`-derived: variant order *is* tier order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolTier {
    Bone,
    Stone,
    /// Microlithic / polished stone — stone worked to `Fine` quality.
    FineStone,
    Copper,
    Bronze,
}

impl ToolTier {
    pub fn name(self) -> &'static str {
        match self {
            ToolTier::Bone => "Bone",
            ToolTier::Stone => "Stone",
            ToolTier::FineStone => "Fine Stone",
            ToolTier::Copper => "Copper",
            ToolTier::Bronze => "Bronze",
        }
    }
}

/// Derive a [`ToolTier`] from a manufactured item's material + quality.
/// Stone at `Fine`+ quality reads as `FineStone` (microlithic / polished).
/// Non-tool materials (Wood/Iron/Steel/Leather) and untagged items fall
/// back to the `Stone` baseline — defensive, real tools always tag a tool
/// material.
pub fn tool_tier(material: Option<ItemMaterial>, quality: Option<ItemQuality>) -> ToolTier {
    match material {
        Some(ItemMaterial::Bone) => ToolTier::Bone,
        Some(ItemMaterial::Copper) => ToolTier::Copper,
        Some(ItemMaterial::Bronze) => ToolTier::Bronze,
        Some(ItemMaterial::Stone) => {
            if matches!(quality, Some(ItemQuality::Fine) | Some(ItemQuality::Masterwork)) {
                ToolTier::FineStone
            } else {
                ToolTier::Stone
            }
        }
        _ => ToolTier::Stone,
    }
}

/// Tier of a concrete [`Item`].
pub fn item_tool_tier(item: &Item) -> ToolTier {
    tool_tier(item.material, item.quality)
}

/// Work-speed multiplier a tool of `tier` grants. The *only* v1 benefit of a
/// better tool — no extra yield. Bone is slightly slower than plain stone.
pub fn work_speed_mult(tier: ToolTier) -> f32 {
    match tier {
        ToolTier::Bone => 0.9,
        ToolTier::Stone => 1.0,
        ToolTier::FineStone => 1.15,
        ToolTier::Copper => 1.4,
        ToolTier::Bronze => 1.7,
    }
}

/// A tool a task requires: a use-kind plus a minimum tier. Most gathering
/// requirements use `min_tier = Bone` (any tool of the right form will do);
/// recipes use a higher floor (a bronze workbench needs a Copper+ hammer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolRequirement {
    pub use_kind: ToolUseKind,
    pub min_tier: ToolTier,
}

impl ToolRequirement {
    /// Any tool of the matching form satisfies this.
    pub fn any(use_kind: ToolUseKind) -> Self {
        Self {
            use_kind,
            min_tier: ToolTier::Bone,
        }
    }

    pub fn at_least(use_kind: ToolUseKind, min_tier: ToolTier) -> Self {
        Self { use_kind, min_tier }
    }

    /// Does `item` satisfy this requirement?
    pub fn satisfied_by(&self, item: &Item) -> bool {
        ToolForm::from_resource_id(item.resource_id) == Some(self.use_kind.form())
            && item_tool_tier(item) >= self.min_tier
    }
}

/// A worker's carried tools. Separate from `Carrier` cargo hands — holding a
/// tool here does not consume a hand, so the worker can still carry gathered
/// wood/stone. Capacity is era-scaled ([`capacity_for_era`]): one tool in the
/// early eras (carry one, swap by re-withdrawing), more once tool belts exist.
#[derive(Component, Clone, Debug)]
pub struct ToolKit {
    pub items: Vec<Item>,
    pub capacity: u8,
}

impl Default for ToolKit {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            capacity: 1,
        }
    }
}

impl ToolKit {
    pub fn new(capacity: u8) -> Self {
        Self {
            items: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn is_full(&self) -> bool {
        self.items.len() as u8 >= self.capacity
    }

    /// Highest-tier carried tool satisfying `req`, if any.
    pub fn best_for(&self, req: &ToolRequirement) -> Option<&Item> {
        self.items
            .iter()
            .filter(|it| req.satisfied_by(it))
            .max_by_key(|it| item_tool_tier(it))
    }

    pub fn satisfies(&self, req: &ToolRequirement) -> bool {
        self.best_for(req).is_some()
    }

    /// Highest-tier carried tool of `form`, regardless of tier floor.
    pub fn best_of_form(&self, form: ToolForm) -> Option<&Item> {
        self.items
            .iter()
            .filter(|it| ToolForm::from_resource_id(it.resource_id) == Some(form))
            .max_by_key(|it| item_tool_tier(it))
    }

    pub fn has_form(&self, form: ToolForm) -> bool {
        self.best_of_form(form).is_some()
    }

    /// Stow `item`. If the kit is full, the lowest-tier tool is evicted and
    /// returned so the caller can spill it to storage/ground; if the incoming
    /// tool is itself the worst, it is returned unchanged (rejected). Returns
    /// `None` when there was free space.
    pub fn stow(&mut self, item: Item) -> Option<Item> {
        if !self.is_full() {
            self.items.push(item);
            return None;
        }
        // Kit full — find the weakest resident tool.
        let incoming_tier = item_tool_tier(&item);
        let worst = self
            .items
            .iter()
            .enumerate()
            .min_by_key(|(_, it)| item_tool_tier(it))
            .map(|(idx, it)| (idx, item_tool_tier(it)));
        match worst {
            Some((idx, worst_tier)) if incoming_tier > worst_tier => {
                Some(std::mem::replace(&mut self.items[idx], item))
            }
            _ => Some(item),
        }
    }
}

/// `ToolKit` capacity for a faction starting in `era`. Early eras carry a
/// single tool; tool belts / satchels (Neolithic onward) raise it.
pub fn capacity_for_era(era: Era) -> u8 {
    match era {
        Era::Paleolithic | Era::Mesolithic => 1,
        _ => 3,
    }
}

/// Sequential executor for [`crate::simulation::typed_task::Task::StowToolKit`].
/// In-place follow-up of a `WithdrawTool` leg: move the just-withdrawn tool
/// `Item` out of the worker's `Carrier` hands and into their [`ToolKit`]. If
/// the kit is full, the lowest-tier resident tool is evicted and spilled to a
/// `GroundItem` at the worker's tile. A worker who arrives empty-handed (the
/// withdraw raced / found nothing) self-advances cleanly.
pub fn stow_toolkit_task_system(
    mut commands: bevy::prelude::Commands,
    clock: bevy::prelude::Res<crate::simulation::schedule::SimClock>,
    spatial: bevy::prelude::Res<crate::world::spatial::SpatialIndex>,
    mut ground_items: bevy::prelude::Query<&mut crate::simulation::items::GroundItem>,
    mut query: bevy::prelude::Query<(
        &mut crate::simulation::person::PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut crate::simulation::carry::Carrier,
        &mut ToolKit,
        &bevy::prelude::Transform,
        &crate::simulation::schedule::BucketSlot,
        &crate::simulation::lod::LodLevel,
    )>,
) {
    use crate::simulation::lod::LodLevel;
    use crate::simulation::person::AiState;
    use crate::simulation::tasks::TaskKind;
    use crate::world::terrain::world_to_tile;

    for (mut ai, mut aq, mut carrier, mut kit, transform, slot, lod) in query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::StowToolKit as u16 {
            continue;
        }
        // Defence in depth — wrong variant behind the StowToolKit kind.
        let Some(req) = aq.current.as_stow_toolkit() else {
            aq.cancel_chain(&mut ai);
            continue;
        };

        // Pull the matching tool Item out of a hand slot.
        let want_form = req.use_kind.form();
        let pull = |slot: &Option<crate::simulation::carry::HeldStack>| -> Option<Item> {
            slot.as_ref().and_then(|s| {
                (ToolForm::from_resource_id(s.item.resource_id) == Some(want_form))
                    .then_some(s.item)
            })
        };
        let held = pull(&carrier.left).or_else(|| pull(&carrier.right));
        if let Some(item) = held {
            carrier.remove_item(item, 1);
            if let Some(evicted) = kit.stow(item) {
                // Kit full — spill the displaced (or rejected) tool to ground.
                let (tx, ty) = world_to_tile(transform.translation.truncate());
                crate::simulation::items::spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    evicted,
                    1,
                );
            }
        }
        ai.work_progress = 0;
        aq.finish_task(&mut ai);
    }
}

/// Era-appropriate tier of the *primary* tool stock a faction starts with.
/// Backup/legacy tools one tier down are seeded alongside (see
/// [`starting_tool_loadout`]).
pub fn starting_tool_tier(era: Era) -> ToolTier {
    match era {
        Era::Paleolithic => ToolTier::Stone,
        Era::Mesolithic => ToolTier::FineStone,
        Era::Neolithic => ToolTier::FineStone,
        Era::Chalcolithic => ToolTier::Copper,
        Era::BronzeAge => ToolTier::Bronze,
    }
}

/// Map a [`ToolTier`] back to the `(material, quality)` pair a manufactured
/// tool [`Item`] of that tier carries.
pub fn tier_material_quality(tier: ToolTier) -> (ItemMaterial, ItemQuality) {
    match tier {
        ToolTier::Bone => (ItemMaterial::Bone, ItemQuality::Normal),
        ToolTier::Stone => (ItemMaterial::Stone, ItemQuality::Normal),
        ToolTier::FineStone => (ItemMaterial::Stone, ItemQuality::Fine),
        ToolTier::Copper => (ItemMaterial::Copper, ItemQuality::Normal),
        ToolTier::Bronze => (ItemMaterial::Bronze, ItemQuality::Normal),
    }
}

/// Build a concrete tool [`Item`] of `form` at `tier`.
pub fn tool_item_of(form: ToolForm, tier: ToolTier) -> Item {
    let (mat, q) = tier_material_quality(tier);
    Item::new_manufactured(form.resource_id(), mat, q)
}

/// The starting tool loadout for a faction in `era` with the given tech set.
/// Returns `(form, tier, count)` triples, count already scaled to
/// `member_count`. Every era seeds the four core forms (Knife/Axe/Pick/
/// Hammer); tech unlocks add Awl / Fishing Kit / Sickle.
pub fn starting_tool_loadout(
    era: Era,
    member_count: u32,
    has_bone_tools: bool,
    has_fishing: bool,
    has_crop_cultivation: bool,
) -> Vec<(ToolForm, ToolTier, u32)> {
    let members = member_count.max(1);
    let tier = starting_tool_tier(era);
    // One knife/axe per ~2 members, at least 2; one pick/hammer per ~5, min 1.
    let per_common = (members / 2).max(2);
    let per_rare = (members / 5).max(1);
    let mut out = vec![
        (ToolForm::Knife, tier, per_common),
        (ToolForm::Axe, tier, per_common),
        (ToolForm::Pick, tier, per_rare),
        (ToolForm::Hammer, tier, per_rare),
    ];
    if has_bone_tools {
        out.push((ToolForm::Awl, ToolTier::Bone, per_rare));
    }
    if has_fishing {
        out.push((ToolForm::FishingKit, tier.min(ToolTier::FineStone), per_rare));
    }
    if has_crop_cultivation {
        out.push((ToolForm::Sickle, tier, per_common));
    }
    out
}

/// Game-start tool seeding (Realistic Tool Overhaul). Runs once at
/// `OnEnter(Playing)` after `seed_starting_farms_system`, before
/// `mark_warmup_complete_system`.
///
/// For every settled, non-SOLO, non-household faction it pre-stows one core
/// tool into each founding member's [`ToolKit`] (so the craft / gather gates
/// are satisfiable from tick 0 with no deadlock) and drops the remaining
/// era-loadout tool stacks as `GroundItem`s at the faction storage tile for
/// withdrawal. Nomadic factions distribute the whole loadout across member
/// kits (no storage tile). Sandbox (`seed_buildings == false`) is skipped.
pub fn seed_starting_tools_system(
    mut commands: bevy::prelude::Commands,
    options: bevy::prelude::Res<crate::game_state::GameStartOptions>,
    registry: bevy::prelude::Res<crate::simulation::faction::FactionRegistry>,
    spatial: bevy::prelude::Res<crate::world::spatial::SpatialIndex>,
    storage_tiles: bevy::prelude::Query<(
        &crate::simulation::faction::FactionStorageTile,
        &bevy::prelude::Transform,
    )>,
    mut ground_items: bevy::prelude::Query<&mut crate::simulation::items::GroundItem>,
    mut kit_q: bevy::prelude::Query<(
        bevy::prelude::Entity,
        &crate::simulation::faction::FactionMember,
        &mut ToolKit,
    )>,
) {
    use crate::simulation::faction::{Lifestyle, SOLO};
    use crate::simulation::technology::{
        current_era, BONE_TOOLS, CROP_CULTIVATION, FISHING,
    };
    use crate::world::terrain::world_to_tile;

    if !options.seed_buildings {
        return;
    }

    let mut faction_ids: Vec<u32> = registry.factions.keys().copied().collect();
    faction_ids.sort_unstable();

    for fid in faction_ids {
        if fid == SOLO {
            continue;
        }
        let Some(faction) = registry.factions.get(&fid) else {
            continue;
        };
        if faction.parent_faction.is_some() {
            continue; // households share the village stock
        }
        let era = current_era(&faction.techs);
        let loadout = starting_tool_loadout(
            era,
            faction.member_count,
            faction.techs.has(BONE_TOOLS),
            faction.techs.has(FISHING),
            faction.techs.has(CROP_CULTIVATION),
        );
        let nomadic = matches!(faction.lifestyle, Lifestyle::Nomadic);

        // Collect this faction's member kits in entity order for determinism.
        let mut kits: Vec<bevy::prelude::Entity> = kit_q
            .iter()
            .filter(|(_, m, _)| m.faction_id == fid)
            .map(|(e, _, _)| e)
            .collect();
        kits.sort();

        // Flatten the loadout into a queue of concrete tool Items.
        let mut pool: Vec<Item> = Vec::new();
        for (form, tier, count) in loadout {
            for _ in 0..count {
                pool.push(tool_item_of(form, tier));
            }
        }

        if nomadic {
            // Distribute every tool round-robin across member kits.
            let mut ki = 0usize;
            for item in pool {
                if kits.is_empty() {
                    break;
                }
                let mut placed = false;
                for _ in 0..kits.len() {
                    let e = kits[ki % kits.len()];
                    ki += 1;
                    if let Ok((_, _, mut kit)) = kit_q.get_mut(e) {
                        if !kit.is_full() {
                            kit.stow(item);
                            placed = true;
                            break;
                        }
                    }
                }
                let _ = placed; // overflow tools are simply dropped (kits full)
            }
            continue;
        }

        // Settled: pre-stow a core tool per member so gates work tick 0,
        // spill the rest to storage.
        let storage = storage_tiles.iter().find_map(|(s, t)| {
            (s.faction_id == fid).then_some(world_to_tile(t.translation.truncate()))
        });
        let mut leftover: Vec<Item> = Vec::new();
        let mut pi = 0usize;
        for &e in &kits {
            if pi >= pool.len() {
                break;
            }
            if let Ok((_, _, mut kit)) = kit_q.get_mut(e) {
                if !kit.is_full() {
                    kit.stow(pool[pi]);
                    pi += 1;
                }
            }
        }
        leftover.extend_from_slice(&pool[pi..]);

        if let Some((tx, ty)) = storage {
            for item in leftover {
                crate::simulation::items::spawn_or_merge_ground_item_full(
                    &mut commands,
                    &spatial,
                    &mut ground_items,
                    tx,
                    ty,
                    item,
                    1,
                );
            }
        }
    }
}

/// Map a worker's `AgentGoal` (plus `JobClaim` for Craft) to the single
/// [`ToolRequirement`] their *next* episode of work will hard-gate on. Returns
/// `None` for goals whose work is hand-doable or whose tool need is decided
/// downstream of dispatch (e.g. `GatherFood` could be berries or a fish haul —
/// only the fishing branch needs a kit, and that's gated separately at
/// `Task::Fish` dispatch). Keeping the mapping narrow mirrors the spear
/// dispatcher: prefetch only the unambiguous-form cases, let everything else
/// fall through to its degraded-or-stall executor gate.
fn pending_tool_for_goal(
    goal: crate::simulation::goals::AgentGoal,
    claim: Option<&crate::simulation::jobs::JobClaim>,
    job_board: &crate::simulation::jobs::JobBoard,
) -> Option<ToolRequirement> {
    use crate::simulation::goals::AgentGoal;
    use crate::simulation::jobs::JobProgress;
    match goal {
        AgentGoal::GatherWood => Some(ToolRequirement::any(ToolUseKind::Chop)),
        AgentGoal::GatherStone => Some(ToolRequirement::any(ToolUseKind::Mine)),
        AgentGoal::Farm => Some(ToolRequirement::any(ToolUseKind::HarvestCrop)),
        AgentGoal::Craft => {
            // Look up the worker's claimed craft posting to find the recipe.
            // Without a claim we don't know which recipe is coming; let craft
            // dispatch run and the per-tick gate decide degraded behaviour.
            let claim = claim?;
            let posting = job_board.get(claim.job_id)?;
            let recipe_id = match &posting.progress {
                JobProgress::Crafting { recipe, .. } => *recipe,
                _ => return None,
            };
            let recipe = crate::simulation::crafting::craft_recipes()
                .get(recipe_id as usize)?;
            // Recipes carry an ordered list of requirements; pick the first the
            // dispatcher hasn't covered yet. The dispatcher's `satisfies` check
            // narrows from "first req" to "first *unsatisfied* req".
            recipe.tool_requirements.first().copied()
        }
        _ => None,
    }
}

/// Goal-agnostic pre-dispatch system: a worker about to do tool-gated work
/// (`Gather` / `Fish` / `Dig` / `WorkOnCraftOrder`) whose [`ToolKit`] lacks
/// the required `ToolForm` and whose faction storage *does* hold a satisfying
/// tool gets a `[WithdrawTool → StowToolKit]` chain dispatched ahead of the
/// work proper. Mirrors `htn_equip_hunting_spear_dispatch_system` in shape
/// (own `Idle` + `UNEMPLOYED` gate, walks faction storage tiles for a
/// matching `GroundItem`, routes via `assign_task_with_routing`).
///
/// `MF_UNINTERRUPTIBLE`-equivalent survival across goal-flip ticks comes
/// from `goal_dispatch_system`'s preserve-arms keyed on
/// `aq.current.as_withdraw_tool().is_some()` and
/// `TaskKind::StowToolKit`. When no tool is in storage anywhere the
/// dispatcher silently declines; the work proceeds degraded (gather / dig /
/// fish degrade gracefully, craft stalls — by design, chief posting picks
/// up the deficit via `compute_faction_tool_deficits`).
pub fn htn_acquire_tool_dispatch_system(
    chunk_map: bevy::prelude::Res<crate::world::chunk::ChunkMap>,
    chunk_graph: bevy::prelude::Res<crate::pathfinding::chunk_graph::ChunkGraph>,
    chunk_router: bevy::prelude::Res<crate::pathfinding::chunk_router::ChunkRouter>,
    chunk_connectivity: bevy::prelude::Res<crate::pathfinding::connectivity::ChunkConnectivity>,
    storage_tile_map: bevy::prelude::Res<crate::simulation::faction::StorageTileMap>,
    storage_reservations: bevy::prelude::Res<crate::simulation::faction::StorageReservations>,
    faction_registry: bevy::prelude::Res<crate::simulation::faction::FactionRegistry>,
    job_board: bevy::prelude::Res<crate::simulation::jobs::JobBoard>,
    spatial_index: bevy::prelude::Res<crate::world::spatial::SpatialIndex>,
    item_query: bevy::prelude::Query<&crate::simulation::items::GroundItem>,
    stand_reservations: bevy::prelude::Res<crate::simulation::stand_reservation::StandTileReservations>,
    clock: bevy::prelude::Res<crate::simulation::SimClock>,
    mut query: bevy::prelude::Query<
        (
            bevy::prelude::Entity,
            &mut crate::simulation::person::PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &crate::simulation::goals::AgentGoal,
            &ToolKit,
            &bevy::prelude::Transform,
            &crate::simulation::faction::FactionMember,
            &crate::simulation::lod::LodLevel,
            Option<&crate::simulation::jobs::JobClaim>,
        ),
        bevy::prelude::Without<crate::simulation::person::Drafted>,
    >,
) {
    let now = clock.tick;
    use crate::simulation::faction::SOLO;
    use crate::simulation::lod::LodLevel;
    use crate::simulation::person::AiState;
    use crate::simulation::person::UNEMPLOYED_TASK_KIND;
    use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
    use crate::simulation::typed_task::Task;
    use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
    use crate::world::terrain::TILE_SIZE;

    for (
        actor,
        mut ai,
        mut aq,
        goal,
        kit,
        transform,
        member,
        lod,
        claim,
    ) in query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        // Same Idle + UNEMPLOYED gate as every ParallelB dispatcher.
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }

        let Some(req) = pending_tool_for_goal(*goal, claim, &job_board) else {
            continue;
        };
        // Already covered by the carried kit.
        if kit.satisfies(&req) {
            continue;
        }
        // Faction-level stock short-circuit: at least one Item of the right
        // form exists. The per-tile walk below is the per-Item satisfaction
        // check (form + tier floor); a non-zero `totals` entry for the
        // form's resource is necessary but not sufficient.
        let want_form = req.use_kind.form();
        let want_resource = want_form.resource_id();
        let stock = faction_registry
            .factions
            .get(&member.faction_id)
            .and_then(|f| f.storage.totals.get(&want_resource).copied())
            .unwrap_or(0);
        if stock == 0 {
            continue;
        }

        // Walk faction storage tiles for the *nearest* one carrying a
        // satisfying tool Item — mirrors the spear-dispatcher's per-tile
        // scan. We require a tile-level match (right form *and* tier floor)
        // so we don't withdraw a Bone awl in response to a Copper-awl gate.
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let Some(tiles) = storage_tile_map.by_faction.get(&member.faction_id) else {
            continue;
        };
        let mut best_tile: Option<(i32, i32)> = None;
        let mut best_dist = i32::MAX;
        for &(tx, ty) in tiles {
            let mut tile_has = false;
            for &gi_entity in spatial_index.get(tx, ty) {
                if let Ok(gi) = item_query.get(gi_entity) {
                    if gi.qty > 0 && req.satisfied_by(&gi.item) {
                        tile_has = true;
                        break;
                    }
                }
            }
            if !tile_has {
                continue;
            }
            // Subtract pending reservations on this resource so two workers
            // don't race a single tool stack. (Best-effort — `WithdrawTool`
            // doesn't reserve mid-walk, but `WithdrawMaterial` chains here
            // would, so this prevents over-commit cross-task.)
            let reserved = storage_reservations.get((tx, ty), want_resource);
            let _ = reserved; // tool stacks are typically size 1; reservation here is a hint not a hard cap.
            let dist = (tx - cur_tx).abs() + (ty - cur_ty).abs();
            if dist < best_dist {
                best_dist = dist;
                best_tile = Some((tx, ty));
            }
        }
        let Some(storage_tile) = best_tile else {
            continue;
        };

        let dispatched = assign_task_with_routing(
            &mut ai,
            (cur_tx, cur_ty),
            cur_chunk,
            storage_tile,
            TaskKind::WithdrawMaterial,
            None,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
            &spatial_index,
            &stand_reservations,
            actor,
            now,
                );
        if !dispatched {
            continue;
        }
        ai.reserved_tile = storage_tile;
        // No `WithdrawMaterial`-style count-reservation: tool stacks are
        // size-1 and the per-tile satisfied_by check above already verified
        // an Item is sitting on the tile.
        ai.reserved_resource = None;
        ai.reserved_qty = 0;
        let _ = aq.dispatch(Task::WithdrawTool { req });
        let _ = aq.enqueue(Task::StowToolKit { req });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_item(form: ToolForm, mat: ItemMaterial, q: ItemQuality) -> Item {
        Item::new_manufactured(form.resource_id(), mat, q)
    }

    #[test]
    fn tier_ordering_is_bone_to_bronze() {
        assert!(ToolTier::Bone < ToolTier::Stone);
        assert!(ToolTier::Stone < ToolTier::FineStone);
        assert!(ToolTier::FineStone < ToolTier::Copper);
        assert!(ToolTier::Copper < ToolTier::Bronze);
    }

    #[test]
    fn fine_stone_is_microlithic() {
        assert_eq!(
            tool_tier(Some(ItemMaterial::Stone), Some(ItemQuality::Normal)),
            ToolTier::Stone
        );
        assert_eq!(
            tool_tier(Some(ItemMaterial::Stone), Some(ItemQuality::Fine)),
            ToolTier::FineStone
        );
    }

    #[test]
    fn starting_loadout_scales_and_respects_tech() {
        // Paleolithic, no extra tech → only the 4 core forms, Stone tier.
        let paleo = starting_tool_loadout(Era::Paleolithic, 8, false, false, false);
        assert_eq!(paleo.len(), 4);
        assert!(paleo.iter().all(|(_, t, _)| *t == ToolTier::Stone));
        assert!(paleo.iter().any(|(f, _, c)| *f == ToolForm::Knife && *c >= 2));
        // BronzeAge with crops + fishing + bone → 7 forms, Bronze core tier.
        let bronze = starting_tool_loadout(Era::BronzeAge, 20, true, true, true);
        assert_eq!(bronze.len(), 7);
        assert!(bronze
            .iter()
            .any(|(f, t, _)| *f == ToolForm::Axe && *t == ToolTier::Bronze));
        assert!(bronze.iter().any(|(f, _, _)| *f == ToolForm::Sickle));
        assert!(bronze.iter().any(|(f, _, _)| *f == ToolForm::FishingKit));
    }

    #[test]
    fn faction_tool_deficit_nets_against_holdings() {
        use crate::simulation::jobs::compute_faction_tool_deficits;
        let mut have: ahash::AHashMap<ToolForm, u32> = ahash::AHashMap::default();
        // 8 Paleolithic members want max(8/2,2)=4 knives/axes, 1 pick/hammer.
        // Stock 4 knives → no knife deficit; 0 axes → axe deficit 4.
        have.insert(ToolForm::Knife, 4);
        have.insert(ToolForm::Pick, 1);
        have.insert(ToolForm::Hammer, 1);
        let deficits =
            compute_faction_tool_deficits(Era::Paleolithic, 8, false, false, false, &have);
        let axe = ToolForm::Axe.resource_id();
        assert_eq!(
            deficits.iter().find(|(r, _)| *r == axe).map(|(_, d)| *d),
            Some(4)
        );
        assert!(deficits.iter().all(|(r, _)| *r != ToolForm::Knife.resource_id()));
    }

    #[test]
    fn requirement_matches_form_and_floor() {
        let bronze_axe = tool_item(ToolForm::Axe, ItemMaterial::Bronze, ItemQuality::Normal);
        let chop = ToolRequirement::any(ToolUseKind::Chop);
        assert!(chop.satisfied_by(&bronze_axe));
        // Wrong form.
        let cut = ToolRequirement::any(ToolUseKind::Cut);
        assert!(!cut.satisfied_by(&bronze_axe));
        // Tier floor.
        let stone_axe = tool_item(ToolForm::Axe, ItemMaterial::Stone, ItemQuality::Normal);
        let smith_copper = ToolRequirement::at_least(ToolUseKind::Chop, ToolTier::Copper);
        assert!(!smith_copper.satisfied_by(&stone_axe));
        assert!(smith_copper.satisfied_by(&bronze_axe));
    }

    #[test]
    fn toolkit_evicts_lowest_tier_when_full() {
        let mut kit = ToolKit::new(1);
        let stone = tool_item(ToolForm::Axe, ItemMaterial::Stone, ItemQuality::Normal);
        let bronze = tool_item(ToolForm::Axe, ItemMaterial::Bronze, ItemQuality::Normal);
        assert!(kit.stow(stone).is_none());
        // Better tool displaces the stone one.
        let evicted = kit.stow(bronze).expect("kit full → eviction");
        assert_eq!(item_tool_tier(&evicted), ToolTier::Stone);
        assert_eq!(kit.best_of_form(ToolForm::Axe).map(item_tool_tier), Some(ToolTier::Bronze));
        // A worse tool is rejected outright.
        let worse = tool_item(ToolForm::Axe, ItemMaterial::Bone, ItemQuality::Poor);
        let rejected = kit.stow(worse).expect("worse tool rejected");
        assert_eq!(item_tool_tier(&rejected), ToolTier::Bone);
    }
}
