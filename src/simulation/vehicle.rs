//! Customizable vehicle system — Phase 1: data model, catalog, validation,
//! stats.
//!
//! Vehicles are freeform designs laid out on a bounded 3D cell grid. Every
//! physics quantity (mass, center of mass, stability, payload, speed) is
//! *derived* from the cell bill-of-materials — height is load-bearing, so a
//! tall narrow design genuinely overturns and a wide low one does not.
//!
//! This module is the data foundation. It defines the component / enum
//! surface, loads `assets/data/vehicles/*.ron` into [`VehicleData`], builds
//! the stock-template [`VehicleDesignRegistry`], and provides the two pure
//! functions every later phase leans on: [`validate_design`] (deterministic,
//! pre-queue) and [`derive_stats`].
//!
//! Phases 2-7 (yard assembly, clearance-aware pathing, rollover, cargo-haul
//! migration, designer UI, chariot combat) integrate this model into the
//! sim. Nothing here is wired into a system yet — see `plans/vehicle-system.md`.
//!
//! Phase 1 deliberately defines the *full* component / enum surface ahead of
//! the systems that consume it, so `#![allow(dead_code)]` covers the
//! not-yet-wired scaffolding (`Vehicle`, `VehicleCrew`, rollover state, …).
#![allow(dead_code)]

use bevy::prelude::*;
use serde::Deserialize;

use crate::economy::core_ids;
use crate::economy::resource_catalog::ResourceId;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::vehicle_path::{footprint_astar, VehicleNode, VehiclePathResult, VehiclePathScratch};
use crate::simulation::animals::{
    AnimalUse, AnimalWorkClaim, DomesticAnimal, DomesticSpecies, Tamed,
};
use crate::simulation::combat::{CombatCooldown, CombatTarget, Health};
use crate::simulation::construction::Blueprint;
use crate::simulation::draftwork::{release_animal_work_claim, TRAINING_THRESHOLD_DRAFT};
use crate::simulation::faction::{FactionMember, FactionRegistry, StorageTileMap};
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::{Equipment, EquipmentSlot, GroundItem};
use crate::simulation::jobs::{
    record_progress_filtered, JobBoard, JobClaim, JobCompletedEvent, JobKind, JobProgress,
};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, Person, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::stats::{self, Stats};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::{
    TechId, ANIMAL_HUSBANDRY, BRONZE_CASTING, HORSE_TAMING, OX_CART, WAR_CHARIOT,
};
use crate::simulation::typed_task::{ActionQueue, Task, UNEMPLOYED_TASK_KIND};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, world_to_tile, TILE_SIZE};
use crate::world::tile::TileKind;

// ── grid bounds ───────────────────────────────────────────────────────────

/// Maximum design grid extent. v1 ancient vehicles are 1 cell tall; the 4-tall
/// ceiling reserves headroom for the deferred tank / siege content.
pub const GRID_MAX_WIDTH: i32 = 6;
pub const GRID_MAX_DEPTH: i32 = 4;
pub const GRID_MAX_HEIGHT: i32 = 4;

// ── stat tunables ─────────────────────────────────────────────────────────

/// Loaded mass an Axle cell supports, per point of its material strength.
const AXLE_SUPPORT_PER_STRENGTH: u32 = 2_000;
/// Same, for a Wheel cell (wheels share the axle's load).
const WHEEL_SUPPORT_PER_STRENGTH: u32 = 400;
/// Same, for a Frame cell (the chassis spreads load).
const FRAME_SUPPORT_PER_STRENGTH: u32 = 300;
/// Rolling resistance coefficient feeding `draft_power_needed`.
const TERRAIN_RESISTANCE: f32 = 0.06;
/// Wheelbase → turn radius multiplier — a longer wheelbase turns wider.
const TURN_RADIUS_FACTOR: f32 = 1.5;
/// Base off-road / road speed caps (tiles/tick) at reference wheel traction.
const BASE_ROAD_SPEED: f32 = 1.4;
const BASE_OFFROAD_SPEED: f32 = 0.8;
/// Reference traction value (a wood wheel) speed caps are normalised against.
const REFERENCE_TRACTION: f32 = 55.0;

// ── enums ─────────────────────────────────────────────────────────────────

/// What a cell *is*. The first ten variants are active in v1; the last four
/// are reserved extension points for the deferred tank / siege content
/// (`plans/vehicle-system-tanks.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VehiclePartKind {
    Frame,
    Deck,
    Wall,
    Axle,
    Wheel,
    Hitch,
    Yoke,
    CargoBay,
    CrewSeat,
    WeaponMount,
    // Reserved — defined so `validate_design`'s 3D hooks compile against the
    // full surface; no `core.ron` part defs ship for these yet.
    Engine,
    Track,
    ArmorPlate,
    Turret,
}

impl VehiclePartKind {
    /// A cell a driver or puller occupies — satisfies the "≥1 control cell"
    /// rule. A Hitch / Yoke is where the puller stands, so it counts.
    pub fn is_control(self) -> bool {
        matches!(
            self,
            VehiclePartKind::CrewSeat | VehiclePartKind::Hitch | VehiclePartKind::Yoke
        )
    }

    /// Load-bearing structural cell — a non-bottom cell may rest on one.
    pub fn is_structural(self) -> bool {
        matches!(
            self,
            VehiclePartKind::Frame
                | VehiclePartKind::Deck
                | VehiclePartKind::Wall
                | VehiclePartKind::Axle
        )
    }

    /// Cells that may "float" (be unsupported from directly below) provided
    /// they touch a supporting cell — turret rings and bolted armour plate.
    pub fn may_cantilever(self) -> bool {
        matches!(self, VehiclePartKind::Turret | VehiclePartKind::ArmorPlate)
    }

    /// How many draft animals a single cell of this kind can harness.
    pub fn draft_capacity(self) -> u8 {
        match self {
            VehiclePartKind::Hitch => 1,
            VehiclePartKind::Yoke => 2,
            _ => 0,
        }
    }
}

/// The role a design is built for. Gates which assembly orders accept it and
/// (Phase 6) whether combat cells are meaningful.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VehiclePurpose {
    /// Bulk hauling — carts and wagons.
    Cargo,
    /// Crew-platform combat — chariots.
    War,
    /// People movement (passenger transport).
    Transport,
}

/// Live operating state of a spawned `Vehicle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleState {
    Parked,
    Moving,
    Loading,
    /// Rollover result (Phase 3) — movement disabled until righted.
    Overturned,
}

/// Why a vehicle can't move / fight, as a bitset (Phase 3 / Phase 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VehicleDisableFlags(pub u8);

impl VehicleDisableFlags {
    pub const MOVEMENT: u8 = 1 << 0;
    pub const STEERING: u8 = 1 << 1;
    pub const CARGO: u8 = 1 << 2;

    pub fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }
    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

// ── grid + cells ──────────────────────────────────────────────────────────

/// One cell of a vehicle design. `material` is an existing catalog
/// `ResourceId`; `durability` is the cell's health ceiling.
#[derive(Clone, Copy, Debug)]
pub struct VehicleCell {
    pub kind: VehiclePartKind,
    pub material: ResourceId,
    pub durability: u16,
}

/// A freeform vehicle body — a sparse set of cells over the bounded 3D grid.
/// One grid Z-cell maps to one world Z-level (clearance is load-bearing).
#[derive(Clone, Debug, Default)]
pub struct VehicleGrid {
    pub cells: Vec<(IVec3, VehicleCell)>,
}

impl VehicleGrid {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn get(&self, pos: IVec3) -> Option<&VehicleCell> {
        self.cells.iter().find(|(p, _)| *p == pos).map(|(_, c)| c)
    }

    pub fn contains(&self, pos: IVec3) -> bool {
        self.cells.iter().any(|(p, _)| *p == pos)
    }

    /// Inclusive `(min, max)` bounding box of occupied cells, or `None` when
    /// the grid is empty.
    pub fn bounds(&self) -> Option<(IVec3, IVec3)> {
        let mut it = self.cells.iter().map(|(p, _)| *p);
        let first = it.next()?;
        let (mut lo, mut hi) = (first, first);
        for p in it {
            lo = lo.min(p);
            hi = hi.max(p);
        }
        Some((lo, hi))
    }
}

/// Stored design (registry entry — never per-entity). `grid` is the
/// authoritative shape; `tech_gates` are the techs a faction must know to
/// assemble it.
#[derive(Clone, Debug)]
pub struct VehicleDesign {
    pub id: VehicleDesignId,
    pub name: String,
    pub grid: VehicleGrid,
    pub allowed_purpose: VehiclePurpose,
    pub required_animals: u8,
    pub tech_gates: Vec<TechId>,
    /// Faction that authored this design; `None` for stock templates.
    pub author_faction: Option<u32>,
    pub revision: u16,
}

/// Stable design identity within the `VehicleDesignRegistry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VehicleDesignId(pub u32);

// ── derived stats ─────────────────────────────────────────────────────────

/// Cached, fully-derived stat block for a design. Every field is a pure
/// function of the grid + materials — see [`derive_stats`].
#[derive(Clone, Copy, Debug)]
pub struct VehicleStats {
    pub empty_mass_g: u32,
    pub max_payload_g: u32,
    /// Rated fully-loaded mass (`empty + max_payload`); runtime updates the
    /// live value as cargo changes.
    pub loaded_mass_g: u32,
    pub draft_power_needed: f32,
    pub wheelbase: f32,
    pub track_width: f32,
    pub ground_pressure: f32,
    pub turn_radius: f32,
    pub road_speed_cap: f32,
    pub offroad_speed_cap: f32,
    /// Number of world Z-levels the body spans (vertical clearance need).
    pub height_z: u8,
    pub center_of_mass: Vec3,
    /// `track_width / center_of_mass.z` — tip resistance. High = stable.
    pub stability: f32,
    /// `support_limit − loaded_mass`; negative means structurally overloaded.
    pub stress_margin: f32,
    /// Distinct XY tiles the footprint occupies.
    pub footprint_area: u32,
}

// ── footprint ─────────────────────────────────────────────────────────────

/// XY footprint offsets pre-rotated for all four cardinal headings. Anchored
/// so the minimum corner is `(0, 0)` at heading 0.
#[derive(Clone, Debug)]
pub struct VehicleFootprint {
    /// Indexed by heading `0..4` (0 = N, then 90° CCW each step).
    pub offsets_by_heading: [Vec<IVec2>; 4],
    pub height_z: u8,
}

impl VehicleFootprint {
    /// Build the footprint from a design grid: the distinct XY cells, rotated
    /// and re-anchored to a non-negative `(0, 0)`-min box per heading.
    pub fn from_grid(grid: &VehicleGrid) -> VehicleFootprint {
        let mut base: Vec<IVec2> = Vec::new();
        for (p, _) in &grid.cells {
            let xy = IVec2::new(p.x, p.y);
            if !base.contains(&xy) {
                base.push(xy);
            }
        }
        let height_z = grid
            .bounds()
            .map(|(lo, hi)| (hi.z - lo.z + 1).max(1) as u8)
            .unwrap_or(1);

        let rot = |v: IVec2| IVec2::new(-v.y, v.x); // 90° CCW
        let mut headings: [Vec<IVec2>; 4] = Default::default();
        let mut cur = base.clone();
        for h in &mut headings {
            *h = anchor_to_origin(&cur);
            cur = cur.iter().map(|&v| rot(v)).collect();
        }
        VehicleFootprint {
            offsets_by_heading: headings,
            height_z,
        }
    }
}

/// Shift a set of offsets so the minimum corner sits at `(0, 0)`.
fn anchor_to_origin(offsets: &[IVec2]) -> Vec<IVec2> {
    if offsets.is_empty() {
        return Vec::new();
    }
    let min_x = offsets.iter().map(|v| v.x).min().unwrap_or(0);
    let min_y = offsets.iter().map(|v| v.y).min().unwrap_or(0);
    offsets
        .iter()
        .map(|v| IVec2::new(v.x - min_x, v.y - min_y))
        .collect()
}

// ── component scaffolding (wired in later phases) ─────────────────────────

/// A spawned vehicle in the world. Wired into the sim in Phase 2.
#[derive(Component, Clone, Debug)]
pub struct Vehicle {
    pub owner_faction: u32,
    pub design_id: VehicleDesignId,
    pub purpose: VehiclePurpose,
    pub heading: u8,
    pub state: VehicleState,
    pub anchor_tile: (i32, i32),
    pub z: i8,
    /// Worker currently driving the vehicle on a cargo haul; `None` while
    /// parked / idle. The cargo-haul dispatcher (Phase 4) picks vehicles
    /// with `hauler == None`.
    pub hauler: Option<Entity>,
}

/// Render marker — `entity_sprites::spawn_vehicle_sprites` attaches the child
/// sprite once and stamps this so it isn't re-attached.
#[derive(Component, Clone, Copy, Debug)]
pub struct VehicleVisual;

/// A planned multi-tile route for a `Vehicle`, authoritative on the `Vehicle`
/// entity (Phase 4 — the vehicle *leads*, crew follow). `path` is a node
/// sequence from `footprint_astar`; `cursor` is the next node to reach. The
/// component is removed by `vehicle_movement_system` when the path completes —
/// its absence is the "vehicle arrived" signal the cargo-haul executor reads.
#[derive(Component, Clone, Debug)]
pub struct VehiclePathFollow {
    pub path: Vec<VehicleNode>,
    pub cursor: usize,
    /// Tip-torque accumulated over this route leg (rollover input).
    pub tip_torque: f32,
}

/// Placed on a person while they are driving / riding a `Vehicle`. While
/// boarded the person does not path on their own — `movement_system` skips
/// them and `vehicle_crew_sync_system` snaps their `Transform` to the vehicle.
#[derive(Component, Clone, Copy, Debug)]
pub struct BoardedVehicle {
    pub vehicle: Entity,
}

/// Cargo carried on a vehicle. Generalises `cart::CartInventory` — same
/// add / take / qty API. Phase 4 migrates cargo hauling onto this.
#[derive(Component, Clone, Debug, Default)]
pub struct VehicleInventory {
    pub items: Vec<(ResourceId, u32)>,
}

impl VehicleInventory {
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

/// Crew aboard a vehicle (Phase 6).
#[derive(Component, Clone, Debug, Default)]
pub struct VehicleCrew {
    pub driver: Option<Entity>,
    pub passengers: Vec<Entity>,
    pub gunners: Vec<Entity>,
}

/// Draft-animal harness state (Phase 4 / 6).
#[derive(Component, Clone, Debug, Default)]
pub struct VehicleDraft {
    pub hitched: Vec<Entity>,
    pub required_animals: u8,
}

/// Per-cell live health mirror + runtime disable bitset (Phase 6). Inserted on
/// every spawned `Vehicle`; combat (`vehicle_combat_system`) resolves attacks
/// against individual cells. `cells` is parallel to the design grid at spawn —
/// each entry starts at the cell material's `durability`.
#[derive(Component, Clone, Debug, Default)]
pub struct VehicleHealth {
    /// `(cell pos, current health)`.
    pub cells: Vec<(IVec3, u16)>,
    /// Combat-driven disable bits (`MOVEMENT` / `CARGO` / `STEERING`).
    pub disabled: VehicleDisableFlags,
}

impl VehicleHealth {
    /// Fresh health block — every cell at its material's durability ceiling.
    pub fn from_design(design: &VehicleDesign) -> Self {
        VehicleHealth {
            cells: design
                .grid
                .cells
                .iter()
                .map(|(p, c)| (*p, c.durability))
                .collect(),
            disabled: VehicleDisableFlags::default(),
        }
    }

    /// Live health of the cell at `pos` (`0` if absent / destroyed).
    pub fn cell_health(&self, pos: IVec3) -> u16 {
        self.cells
            .iter()
            .find(|(p, _)| *p == pos)
            .map(|(_, h)| *h)
            .unwrap_or(0)
    }

    /// Count of cells still standing (`health > 0`).
    pub fn intact_cells(&self) -> usize {
        self.cells.iter().filter(|(_, h)| *h > 0).count()
    }

    /// True once a wheel/axle hit has set the movement-disable bit.
    pub fn movement_disabled(&self) -> bool {
        self.disabled.has(VehicleDisableFlags::MOVEMENT)
    }
}

/// Number of `CrewSeat` cells in a design — the crew capacity.
pub fn crew_seat_count(design: &VehicleDesign) -> u8 {
    design
        .grid
        .cells
        .iter()
        .filter(|(_, c)| c.kind == VehiclePartKind::CrewSeat)
        .count()
        .min(255) as u8
}

// ── design validation ─────────────────────────────────────────────────────

/// A reason a design grid is not assemblable. `validate_design` returns the
/// full set so a designer UI can surface every problem at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignError {
    /// No cells at all.
    Empty,
    /// A cell sits outside the bounded design grid.
    OutOfBounds(IVec3),
    /// The body is not one connected component (6-neighbour 3D).
    Disconnected,
    /// A non-bottom cell has no support directly below and may not cantilever.
    FloatingCell(IVec3),
    /// A Wheel cell is not adjacent to any Axle cell.
    UnsupportedWheel(IVec3),
    /// No CrewSeat / Hitch / Yoke cell — nothing can drive it.
    NoControlCell,
    /// The design needs draft animals but lacks the Hitch / Yoke capacity.
    BadHitch,
    /// Axle + wheel + frame strength can't even carry the empty chassis.
    OverloadedAxle,
    /// A CargoBay cell can't be reached from a deck / open side.
    BlockedCargo(IVec3),
    /// A `War`-purpose design with no CrewSeat — no crew to fight from.
    ChariotRule,
}

const NEIGHBORS_6: [IVec3; 6] = [
    IVec3::new(1, 0, 0),
    IVec3::new(-1, 0, 0),
    IVec3::new(0, 1, 0),
    IVec3::new(0, -1, 0),
    IVec3::new(0, 0, 1),
    IVec3::new(0, 0, -1),
];

/// Deterministically validate a stored design.
pub fn validate_design(design: &VehicleDesign, data: &VehicleData) -> Result<(), Vec<DesignError>> {
    validate_grid(
        &design.grid,
        design.allowed_purpose,
        design.required_animals,
        data,
    )
}

/// Validate a raw grid. `validate_design` is the thin wrapper; the designer
/// UI (Phase 5) calls this directly on the in-progress grid.
pub fn validate_grid(
    grid: &VehicleGrid,
    purpose: VehiclePurpose,
    required_animals: u8,
    data: &VehicleData,
) -> Result<(), Vec<DesignError>> {
    let mut errors: Vec<DesignError> = Vec::new();

    if grid.is_empty() {
        return Err(vec![DesignError::Empty]);
    }

    // Bounds.
    for (p, _) in &grid.cells {
        if p.x < 0
            || p.y < 0
            || p.z < 0
            || p.x >= GRID_MAX_WIDTH
            || p.y >= GRID_MAX_DEPTH
            || p.z >= GRID_MAX_HEIGHT
        {
            errors.push(DesignError::OutOfBounds(*p));
        }
    }

    // Connectivity — flood fill over 6-neighbours from the first cell.
    {
        let mut seen: Vec<IVec3> = Vec::with_capacity(grid.cells.len());
        let mut stack: Vec<IVec3> = vec![grid.cells[0].0];
        while let Some(p) = stack.pop() {
            if seen.contains(&p) {
                continue;
            }
            seen.push(p);
            for d in NEIGHBORS_6 {
                let n = p + d;
                if grid.contains(n) && !seen.contains(&n) {
                    stack.push(n);
                }
            }
        }
        if seen.len() != grid.cells.len() {
            errors.push(DesignError::Disconnected);
        }
    }

    let min_z = grid.bounds().map(|(lo, _)| lo.z).unwrap_or(0);

    // Floating cells — every non-bottom cell rests on a cell directly below,
    // unless it may cantilever (Turret / ArmorPlate) off a supporting cell.
    for (p, cell) in &grid.cells {
        if p.z <= min_z {
            continue;
        }
        let supported_below = grid.contains(*p + IVec3::new(0, 0, -1));
        if supported_below {
            continue;
        }
        let cantilever_ok = cell.kind.may_cantilever()
            && NEIGHBORS_6.iter().any(|d| {
                grid.get(*p + *d)
                    .map(|c| c.kind.is_structural())
                    .unwrap_or(false)
            });
        if !cantilever_ok {
            errors.push(DesignError::FloatingCell(*p));
        }
    }

    // Every Wheel adjacent to an Axle.
    for (p, cell) in &grid.cells {
        if cell.kind != VehiclePartKind::Wheel {
            continue;
        }
        let near_axle = NEIGHBORS_6.iter().any(|d| {
            grid.get(*p + *d)
                .map(|c| c.kind == VehiclePartKind::Axle)
                .unwrap_or(false)
        });
        if !near_axle {
            errors.push(DesignError::UnsupportedWheel(*p));
        }
    }

    // At least one control cell.
    if !grid.cells.iter().any(|(_, c)| c.kind.is_control()) {
        errors.push(DesignError::NoControlCell);
    }

    // Draft capacity matches the required animal count.
    if required_animals > 0 {
        let capacity: u32 = grid
            .cells
            .iter()
            .map(|(_, c)| c.kind.draft_capacity() as u32)
            .sum();
        if capacity < required_animals as u32 {
            errors.push(DesignError::BadHitch);
        }
    }

    // Axles support the empty chassis.
    let stats = derive_stats(grid, data);
    if stats.stress_margin < 0.0 && (stats.empty_mass_g as f32) > support_limit_g(grid, data) as f32
    {
        errors.push(DesignError::OverloadedAxle);
    }

    // CargoBay reachability — flood from any XY-edge or deck/frame-adjacent
    // bay cell through neighbouring bay cells; an unreached bay is blocked.
    {
        let bays: Vec<IVec3> = grid
            .cells
            .iter()
            .filter(|(_, c)| c.kind == VehiclePartKind::CargoBay)
            .map(|(p, _)| *p)
            .collect();
        if !bays.is_empty() {
            let (lo, hi) = grid.bounds().unwrap();
            let edge_or_open = |p: IVec3| -> bool {
                if p.x == lo.x || p.x == hi.x || p.y == lo.y || p.y == hi.y {
                    return true;
                }
                NEIGHBORS_6.iter().any(|d| {
                    grid.get(p + *d)
                        .map(|c| {
                            matches!(c.kind, VehiclePartKind::Deck | VehiclePartKind::Frame)
                        })
                        .unwrap_or(false)
                })
            };
            let mut reachable: Vec<IVec3> = bays.iter().copied().filter(|p| edge_or_open(*p)).collect();
            let mut frontier = reachable.clone();
            while let Some(p) = frontier.pop() {
                for d in NEIGHBORS_6 {
                    let n = p + d;
                    if bays.contains(&n) && !reachable.contains(&n) {
                        reachable.push(n);
                        frontier.push(n);
                    }
                }
            }
            for b in &bays {
                if !reachable.contains(b) {
                    errors.push(DesignError::BlockedCargo(*b));
                }
            }
        }
    }

    // Chariot rule — a War design must carry crew.
    if purpose == VehiclePurpose::War
        && !grid
            .cells
            .iter()
            .any(|(_, c)| c.kind == VehiclePartKind::CrewSeat)
    {
        errors.push(DesignError::ChariotRule);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ── stat derivation ───────────────────────────────────────────────────────

/// Per-cell structural mass (grams) — part reference mass scaled by the
/// material's density.
fn cell_mass_g(cell: &VehicleCell, data: &VehicleData) -> u32 {
    let base = data
        .part(cell.kind)
        .map(|p| p.base_mass_g)
        .unwrap_or(2_000);
    let density = data
        .material(cell.material)
        .map(|m| m.density_pct)
        .unwrap_or(100);
    ((base as u64 * density as u64) / 100) as u32
}

/// Total structural support (grams) from axle + wheel + frame cells.
fn support_limit_g(grid: &VehicleGrid, data: &VehicleData) -> u32 {
    let mut total: u32 = 0;
    for (_, cell) in &grid.cells {
        let strength = data
            .material(cell.material)
            .map(|m| m.strength as u32)
            .unwrap_or(30);
        let per = match cell.kind {
            VehiclePartKind::Axle => AXLE_SUPPORT_PER_STRENGTH,
            VehiclePartKind::Wheel => WHEEL_SUPPORT_PER_STRENGTH,
            VehiclePartKind::Frame => FRAME_SUPPORT_PER_STRENGTH,
            _ => 0,
        };
        total = total.saturating_add(strength.saturating_mul(per));
    }
    total
}

/// Derive the full [`VehicleStats`] block from a design grid + materials.
/// Pure and deterministic — the designer UI's live preview and the assembly
/// pipeline call the same function.
pub fn derive_stats(grid: &VehicleGrid, data: &VehicleData) -> VehicleStats {
    if grid.is_empty() {
        return VehicleStats {
            empty_mass_g: 0,
            max_payload_g: 0,
            loaded_mass_g: 0,
            draft_power_needed: 0.0,
            wheelbase: 0.0,
            track_width: 0.0,
            ground_pressure: 0.0,
            turn_radius: 0.0,
            road_speed_cap: 0.0,
            offroad_speed_cap: 0.0,
            height_z: 0,
            center_of_mass: Vec3::ZERO,
            stability: 0.0,
            stress_margin: 0.0,
            footprint_area: 0,
        };
    }

    let (lo, hi) = grid.bounds().unwrap();

    // Mass + center of mass (cell centre = grid coord + 0.5).
    let mut empty_mass: u64 = 0;
    let mut moment = Vec3::ZERO;
    for (p, cell) in &grid.cells {
        let m = cell_mass_g(cell, data) as u64;
        empty_mass += m;
        let centre = Vec3::new(p.x as f32 + 0.5, p.y as f32 + 0.5, p.z as f32 + 0.5);
        moment += centre * m as f32;
    }
    let empty_mass_g = empty_mass as u32;
    let center_of_mass = if empty_mass > 0 {
        moment / empty_mass as f32
    } else {
        Vec3::ZERO
    };

    // Footprint geometry.
    let mut footprint: Vec<IVec2> = Vec::new();
    for (p, _) in &grid.cells {
        let xy = IVec2::new(p.x, p.y);
        if !footprint.contains(&xy) {
            footprint.push(xy);
        }
    }
    let footprint_area = footprint.len().max(1) as u32;

    // Track width — distinct X of wheel cells, falling back to the bbox width.
    let mut wheel_x: Vec<i32> = Vec::new();
    let mut axle_y: Vec<i32> = Vec::new();
    let mut wheel_traction_sum: f32 = 0.0;
    let mut wheel_count: u32 = 0;
    for (p, cell) in &grid.cells {
        match cell.kind {
            VehiclePartKind::Wheel => {
                if !wheel_x.contains(&p.x) {
                    wheel_x.push(p.x);
                }
                wheel_traction_sum += data
                    .material(cell.material)
                    .map(|m| m.traction as f32)
                    .unwrap_or(40.0);
                wheel_count += 1;
            }
            VehiclePartKind::Axle => {
                if !axle_y.contains(&p.y) {
                    axle_y.push(p.y);
                }
            }
            _ => {}
        }
    }
    let track_width = if wheel_x.len() >= 2 {
        (wheel_x.iter().max().unwrap() - wheel_x.iter().min().unwrap() + 1) as f32
    } else {
        (hi.x - lo.x + 1) as f32
    };
    let wheelbase = if axle_y.len() >= 2 {
        (axle_y.iter().max().unwrap() - axle_y.iter().min().unwrap() + 1) as f32
    } else {
        (hi.y - lo.y + 1) as f32
    };

    let height_z = (hi.z - lo.z + 1).max(1) as u8;

    // Support → payload.
    let support = support_limit_g(grid, data);
    let cargo_volume: u32 = grid
        .cells
        .iter()
        .filter(|(_, c)| c.kind == VehiclePartKind::CargoBay)
        .map(|(_, c)| {
            data.part(c.kind)
                .map(|p| p.cargo_volume_g)
                .unwrap_or(0)
        })
        .sum();
    let max_payload_g = cargo_volume.min(support.saturating_sub(empty_mass_g));
    let loaded_mass_g = empty_mass_g.saturating_add(max_payload_g);

    // Stability — wide & low resists tipping; tall & narrow does not.
    let stability = track_width / center_of_mass.z.max(0.01);
    let stress_margin = support as f32 - loaded_mass_g as f32;

    // Speed caps scale with wheel material traction.
    let wheel_quality = if wheel_count > 0 {
        (wheel_traction_sum / wheel_count as f32) / REFERENCE_TRACTION
    } else {
        0.5
    };
    let road_speed_cap = BASE_ROAD_SPEED * wheel_quality;
    let offroad_speed_cap = BASE_OFFROAD_SPEED * wheel_quality;

    let draft_power_needed = loaded_mass_g as f32 * TERRAIN_RESISTANCE / wheel_quality.max(0.1);
    let turn_radius = wheelbase * TURN_RADIUS_FACTOR;
    let ground_pressure = loaded_mass_g as f32 / footprint_area as f32;

    VehicleStats {
        empty_mass_g,
        max_payload_g,
        loaded_mass_g,
        draft_power_needed,
        wheelbase,
        track_width,
        ground_pressure,
        turn_radius,
        road_speed_cap,
        offroad_speed_cap,
        height_z,
        center_of_mass,
        stability,
        stress_margin,
        footprint_area,
    }
}

// ── RON catalog ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
#[serde(rename = "MaterialProfile")]
struct MaterialProfileRon {
    resource: String,
    density_pct: u32,
    strength: u16,
    friction: u16,
    traction: u16,
    durability: u16,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename = "PartDef")]
struct PartDefRon {
    kind: VehiclePartKind,
    base_mass_g: u32,
    cargo_volume_g: u32,
    crew_capacity: u8,
    #[serde(default)]
    tech_gates: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename = "CellDef")]
struct CellDefRon {
    x: i32,
    y: i32,
    z: i32,
    kind: VehiclePartKind,
    material: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename = "TemplateDef")]
struct TemplateDefRon {
    name: String,
    purpose: VehiclePurpose,
    required_animals: u8,
    #[serde(default)]
    tech_gates: Vec<String>,
    cells: Vec<CellDefRon>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "VehicleDataFile")]
struct VehicleDataFile {
    #[serde(default)]
    materials: Vec<MaterialProfileRon>,
    #[serde(default)]
    parts: Vec<PartDefRon>,
    #[serde(default)]
    templates: Vec<TemplateDefRon>,
}

/// Resolved material profile — physics inputs for one cell material.
#[derive(Clone, Copy, Debug)]
pub struct MaterialProfile {
    pub resource: ResourceId,
    pub density_pct: u32,
    pub strength: u16,
    pub friction: u16,
    pub traction: u16,
    pub durability: u16,
}

/// Resolved part definition — per-cell structural inputs.
#[derive(Clone, Debug)]
pub struct PartDef {
    pub kind: VehiclePartKind,
    pub base_mass_g: u32,
    pub cargo_volume_g: u32,
    pub crew_capacity: u8,
    pub tech_gates: Vec<TechId>,
}

/// Loaded vehicle catalog — material profiles + part definitions. The
/// physics surface every later phase reads. Inserted as a Bevy resource at
/// `WorldPlugin::build`.
#[derive(Resource, Clone, Debug, Default)]
pub struct VehicleData {
    materials: Vec<MaterialProfile>,
    parts: Vec<PartDef>,
}

impl VehicleData {
    pub fn material(&self, rid: ResourceId) -> Option<&MaterialProfile> {
        self.materials.iter().find(|m| m.resource == rid)
    }

    pub fn part(&self, kind: VehiclePartKind) -> Option<&PartDef> {
        self.parts.iter().find(|p| p.kind == kind)
    }

    /// Default material for fresh designs / stock-part assembly — wood.
    pub fn default_material(&self) -> ResourceId {
        core_ids::wood()
    }

    pub fn material_count(&self) -> usize {
        self.materials.len()
    }

    /// All loaded material profiles — the designer UI's material picker.
    pub fn materials(&self) -> &[MaterialProfile] {
        &self.materials
    }

    /// All loaded part definitions — the designer UI's part palette.
    pub fn parts(&self) -> &[PartDef] {
        &self.parts
    }
}

/// Durability ceiling for a fresh cell of `material` — the material's
/// catalog durability with a graceful fallback. Shared by the catalog
/// loader and the designer UI so a hand-built cell matches a stock one.
pub fn cell_durability(material: ResourceId, data: &VehicleData) -> u16 {
    data.material(material).map(|m| m.durability).unwrap_or(100)
}

/// All known vehicle designs, keyed by id. Seeded from the RON stock
/// templates; Phase 5 adds player / AI freeform designs.
#[derive(Resource, Clone, Debug, Default)]
pub struct VehicleDesignRegistry {
    designs: Vec<VehicleDesign>,
    next_id: u32,
}

impl VehicleDesignRegistry {
    pub fn get(&self, id: VehicleDesignId) -> Option<&VehicleDesign> {
        self.designs.iter().find(|d| d.id == id)
    }

    pub fn by_name(&self, name: &str) -> Option<&VehicleDesign> {
        self.designs.iter().find(|d| d.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &VehicleDesign> {
        self.designs.iter()
    }

    pub fn len(&self) -> usize {
        self.designs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.designs.is_empty()
    }

    /// Register a design, assigning a fresh id. Returns the assigned id.
    pub fn insert(&mut self, mut design: VehicleDesign) -> VehicleDesignId {
        let id = VehicleDesignId(self.next_id);
        self.next_id += 1;
        design.id = id;
        self.designs.push(design);
        id
    }
}

/// Map a snake_case tech key (as written in `core.ron`) to its `TechId`.
fn tech_id_from_name(name: &str) -> Option<TechId> {
    match name {
        "animal_husbandry" => Some(ANIMAL_HUSBANDRY),
        "ox_cart" => Some(OX_CART),
        "horse_taming" => Some(HORSE_TAMING),
        "war_chariot" => Some(WAR_CHARIOT),
        "bronze_casting" => Some(BRONZE_CASTING),
        _ => None,
    }
}

/// Load every `*.ron` under `assets/data/vehicles/` and build the catalog +
/// stock-design registry. Mirrors `archetype::load_archetype_registry`:
/// startup-time failure (missing dir / parse error) is a hard panic, but a
/// material whose backing resource is absent from the catalog is *skipped*
/// (tolerant) so a partial resource catalog can't brick startup.
pub fn load_vehicle_assets() -> (VehicleData, VehicleDesignRegistry) {
    let dir = std::path::Path::new("assets/data/vehicles");
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "VehicleData: cannot read {:?}: {}. Vehicle definitions must \
             live in assets/data/vehicles/*.ron.",
            dir, e
        )
    });

    let catalog = core_ids::catalog();
    let mut data = VehicleData::default();
    let mut templates: Vec<TemplateDefRon> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("VehicleData: cannot read {:?}: {}", path, e));
        let file: VehicleDataFile = ron::from_str(&body)
            .unwrap_or_else(|e| panic!("VehicleData: parse error in {:?}: {}", path, e));

        for m in file.materials {
            match catalog.id_of(&m.resource) {
                Some(rid) => data.materials.push(MaterialProfile {
                    resource: rid,
                    density_pct: m.density_pct,
                    strength: m.strength,
                    friction: m.friction,
                    traction: m.traction,
                    durability: m.durability,
                }),
                None => eprintln!(
                    "VehicleData: skipping material {:?} — no such catalog resource.",
                    m.resource
                ),
            }
        }
        for p in file.parts {
            let tech_gates = p
                .tech_gates
                .iter()
                .filter_map(|n| tech_id_from_name(n))
                .collect();
            data.parts.push(PartDef {
                kind: p.kind,
                base_mass_g: p.base_mass_g,
                cargo_volume_g: p.cargo_volume_g,
                crew_capacity: p.crew_capacity,
                tech_gates,
            });
        }
        templates.extend(file.templates);
    }

    if data.parts.is_empty() {
        panic!(
            "VehicleData: no part definitions found in {:?}. At least one \
             part definition is required.",
            dir
        );
    }

    // Build the stock-design registry from the templates.
    let mut registry = VehicleDesignRegistry::default();
    for t in templates {
        let mut grid = VehicleGrid::default();
        for c in &t.cells {
            let material = catalog
                .id_of(&c.material)
                .unwrap_or_else(|| data.default_material());
            let durability = data
                .material(material)
                .map(|m| m.durability)
                .unwrap_or(100);
            grid.cells.push((
                IVec3::new(c.x, c.y, c.z),
                VehicleCell {
                    kind: c.kind,
                    material,
                    durability,
                },
            ));
        }
        let tech_gates = t
            .tech_gates
            .iter()
            .filter_map(|n| tech_id_from_name(n))
            .collect();
        registry.insert(VehicleDesign {
            id: VehicleDesignId(0), // reassigned by `insert`
            name: t.name,
            grid,
            allowed_purpose: t.purpose,
            required_animals: t.required_animals,
            tech_gates,
            author_faction: None,
            revision: 0,
        });
    }

    (data, registry)
}

// ── Phase 2: VehicleYard, assembly queue, assembly system ─────────────────

/// Cadence of `vehicle_assembly_system` (ticks). Queued vehicles assemble
/// within roughly this window once their bill is affordable.
const ASSEMBLY_CADENCE_TICKS: u64 = 60;

/// A built vehicle yard — the assembly + parking anchor. Single-tile,
/// finalised by `construction_system`; tile-indexed in `VehicleYardMap` via
/// the `on_add` / `on_remove` hooks (the `PenMap` pattern).
#[derive(Component, Clone, Copy, Debug)]
pub struct VehicleYard {
    pub faction_id: u32,
    pub tile: (i32, i32),
}

/// Tile → `VehicleYard` entity index.
#[derive(Resource, Default)]
pub struct VehicleYardMap(pub ahash::AHashMap<(i32, i32), Entity>);

pub fn on_vehicle_yard_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(yard) = world.get::<VehicleYard>(entity).copied() else {
        return;
    };
    world
        .resource_mut::<VehicleYardMap>()
        .0
        .insert(yard.tile, entity);
}

pub fn on_vehicle_yard_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(yard) = world.get::<VehicleYard>(entity).copied() else {
        return;
    };
    let mut map = world.resource_mut::<VehicleYardMap>();
    if map.0.get(&yard.tile).copied() == Some(entity) {
        map.0.remove(&yard.tile);
    }
}

/// Pending vehicle-assembly orders, drained by `vehicle_assembly_system`.
/// Each entry is `(faction_id, design_id)`. Player orders land here via
/// `PlayerCommand::QueueVehicle`; AI auto-queues arrive in Phase 5.
#[derive(Resource, Default)]
pub struct VehicleAssemblyQueue {
    pub entries: Vec<(u32, VehicleDesignId)>,
}

/// Compute the raw-resource bill to assemble a design: one unit of each
/// cell's material plus one `tools` per mechanical cell (Wheel / Axle /
/// WeaponMount). Grouped per `ResourceId` — never explodes the catalog
/// (every material is an existing catalog resource).
pub fn design_bill(design: &VehicleDesign) -> Vec<(ResourceId, u32)> {
    let tools = core_ids::tools();
    let mut bill: Vec<(ResourceId, u32)> = Vec::new();
    let mut add = |rid: ResourceId, n: u32| {
        if let Some(slot) = bill.iter_mut().find(|(r, _)| *r == rid) {
            slot.1 += n;
        } else {
            bill.push((rid, n));
        }
    };
    for (_, cell) in &design.grid.cells {
        add(cell.material, 1);
        if matches!(
            cell.kind,
            VehiclePartKind::Wheel | VehiclePartKind::Axle | VehiclePartKind::WeaponMount
        ) {
            add(tools, 1);
        }
    }
    bill
}

/// Total stock of `rid` across a faction's storage tiles.
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

/// Consume `qty` of `rid` from a faction's storage tiles (caller pre-checks
/// stock for an all-or-nothing buy). Returns the amount actually consumed.
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

/// Spawn a parked `Vehicle` entity (+ inventory / crew / draft components)
/// at `tile`. Phase 3 attaches `VehicleOccupancyIndex` registration.
fn spawn_vehicle(
    commands: &mut Commands,
    faction_id: u32,
    design: &VehicleDesign,
    tile: (i32, i32),
) -> Entity {
    let wp = tile_to_world(tile.0, tile.1);
    commands
        .spawn((
            Vehicle {
                owner_faction: faction_id,
                design_id: design.id,
                purpose: design.allowed_purpose,
                heading: 0,
                state: VehicleState::Parked,
                anchor_tile: tile,
                z: 0,
                hauler: None,
            },
            VehicleInventory::default(),
            VehicleCrew::default(),
            VehicleDraft {
                hitched: Vec::new(),
                required_animals: design.required_animals,
            },
            VehicleHealth::from_design(design),
            Transform::from_xyz(wp.x, wp.y, 0.25),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ))
        .id()
}

/// Economy system (cadence-gated): drains `VehicleAssemblyQueue`. For each
/// order, finds the faction's `VehicleYard`, checks the design's resource
/// bill against faction storage, and on success consumes the bill and spawns
/// a parked `Vehicle`. Orders whose faction has no yard are dropped; orders
/// short of resources stay queued for a later pass.
pub fn vehicle_assembly_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut queue: ResMut<VehicleAssemblyQueue>,
    registry: Res<VehicleDesignRegistry>,
    storage_tile_map: Res<StorageTileMap>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    yards: Query<&VehicleYard>,
) {
    if clock.tick % ASSEMBLY_CADENCE_TICKS != 0 || queue.entries.is_empty() {
        return;
    }
    let mut keep: Vec<(u32, VehicleDesignId)> = Vec::new();
    for (faction_id, design_id) in std::mem::take(&mut queue.entries) {
        let Some(design) = registry.get(design_id).cloned() else {
            continue; // unknown design — drop
        };
        let Some(yard) = yards.iter().find(|y| y.faction_id == faction_id).copied() else {
            continue; // no yard — drop the order
        };
        let bill = design_bill(&design);
        let affordable = bill.iter().all(|&(rid, need)| {
            faction_storage_stock(faction_id, rid, &storage_tile_map, &spatial, &ground_items)
                >= need
        });
        if !affordable {
            keep.push((faction_id, design_id)); // retry next pass
            continue;
        }
        for &(rid, need) in &bill {
            consume_faction_storage(
                &mut commands,
                faction_id,
                rid,
                need,
                &storage_tile_map,
                &spatial,
                &mut ground_items,
            );
        }
        spawn_vehicle(&mut commands, faction_id, &design, yard.tile);
    }
    queue.entries = keep;
}

// ── Phase 3: occupancy index, clearance, rollover ─────────────────────────

/// 2D tile → `Vehicle` entity index. A footprint tile is exclusive
/// regardless of Z — two vehicles never share a tile. Rebuilt every tick by
/// `vehicle_occupancy_sync_system`; the clearance-aware pathfinder treats an
/// entry as a hard block.
#[derive(Resource, Default)]
pub struct VehicleOccupancyIndex(pub ahash::AHashMap<(i32, i32), Entity>);

/// The XY tiles a design occupies when parked at `anchor` facing `heading`.
pub fn footprint_tiles(
    design: &VehicleDesign,
    anchor: (i32, i32),
    heading: u8,
) -> Vec<(i32, i32)> {
    let fp = VehicleFootprint::from_grid(&design.grid);
    fp.offsets_by_heading[(heading % 4) as usize]
        .iter()
        .map(|o| (anchor.0 + o.x, anchor.1 + o.y))
        .collect()
}

/// Sequential system (after movement): full rebuild of the occupancy index.
/// Vehicles are few, so a clear-and-restamp is cheaper than incremental
/// bookkeeping and is despawn-correct by construction.
pub fn vehicle_occupancy_sync_system(
    mut index: ResMut<VehicleOccupancyIndex>,
    registry: Res<VehicleDesignRegistry>,
    vehicles: Query<(Entity, &Vehicle)>,
) {
    index.0.clear();
    for (e, v) in vehicles.iter() {
        let Some(design) = registry.get(v.design_id) else {
            continue;
        };
        for tile in footprint_tiles(design, v.anchor_tile, v.heading) {
            index.0.insert(tile, e);
        }
    }
}

// ── rollover ──────────────────────────────────────────────────────────────

/// Tip-torque tunables. Multiplied by the vehicle's COM height — a tall
/// vehicle converts the same disturbance into far more overturning torque.
const TURN_TORQUE_WEIGHT: f32 = 0.4;
const SLOPE_TORQUE_WEIGHT: f32 = 0.5;
const ROUGH_TORQUE_WEIGHT: f32 = 0.3;
const OVERLOAD_TORQUE_WEIGHT: f32 = 0.6;

/// The disturbances acting on a vehicle during one movement step.
#[derive(Clone, Copy, Debug, Default)]
pub struct RolloverContext {
    /// Turn sharpness this step: `0` straight, `1` at the `turn_radius`
    /// limit, `> 1` sharper than the vehicle should turn.
    pub turn_sharpness: f32,
    /// Absolute terrain Z-change on the step.
    pub z_slope: i32,
    /// Rough terrain underfoot (marsh / sand / scrub).
    pub rough_terrain: bool,
    /// `loaded_mass > max_payload` — the vehicle is over its rated load.
    pub overloaded: bool,
}

/// Overturning torque produced by one movement step. Scaled by COM height so
/// a tall narrow design genuinely tips and a wide low one barely registers.
pub fn step_tip_torque(stats: &VehicleStats, ctx: &RolloverContext) -> f32 {
    let disturbance = ctx.turn_sharpness.max(0.0) * TURN_TORQUE_WEIGHT
        + ctx.z_slope.max(0) as f32 * SLOPE_TORQUE_WEIGHT
        + if ctx.rough_terrain { ROUGH_TORQUE_WEIGHT } else { 0.0 }
        + if ctx.overloaded { OVERLOAD_TORQUE_WEIGHT } else { 0.0 };
    stats.center_of_mass.z.max(0.0) * disturbance
}

/// True when accumulated tip-torque has overcome the vehicle's `stability`.
/// `stability` is `track_width / com_height`, so a wide low vehicle resists
/// far more torque before overturning than a tall narrow one.
pub fn vehicle_rolls_over(stats: &VehicleStats, accumulated_torque: f32) -> bool {
    accumulated_torque > stats.stability
}

// ── Phase 4: cargo hauling — vehicle-leads movement ───────────────────────
//
// A `Vehicle` whose design has cargo capacity ferries bulk construction
// material from faction storage into a blueprint. Unlike the retired
// worker-driven cart, the **vehicle is the authoritative mover**: it owns a
// `VehiclePathFollow` planned by the heading-aware `footprint_astar`
// (clearance- + occupancy-checked); the driver and hitched animals ride it.
//
// 1. **Dispatcher (`htn_vehicle_haul_dispatch_system`, ParallelB).** For each
//    `JobClaim::Haul` holder whose posting still needs `>=
//    VEHICLE_HAUL_MIN_REMAINING` units, picks an idle owner-faction cargo
//    vehicle (+ trained draft animals), and routes the worker on foot to the
//    vehicle so they can board it.
// 2. **Executor (`vehicle_cargo_haul_task_system`, Sequential).** On arrival
//    the worker boards (`BoardedVehicle`); the executor then drives the
//    two-phase load/deliver state machine by planning the vehicle's
//    `footprint_astar` route to storage / the blueprint and resolving the
//    cargo transfer once the vehicle arrives.
// 3. **`vehicle_movement_system`** steps the vehicle along its route;
//    **`vehicle_rollover_system`** overturns an unstable one;
//    **`vehicle_crew_sync_system`** snaps the boarded crew + draft animals to
//    the vehicle each tick.

/// Minimum un-delivered quantity on a `JobProgress::Haul` posting before a
/// vehicle is worth hitching — below this the per-trip hitch overhead isn't
/// amortised and the worker hand-carries instead.
pub const VEHICLE_HAUL_MIN_REMAINING: u32 = 12;

/// TTL backstop on a draft `AnimalWorkClaim`. The executor explicit-releases
/// on completion; this only matters if the worker dies mid-haul.
pub const VEHICLE_CLAIM_TTL_TICKS: u32 = (TICKS_PER_DAY as u32).saturating_mul(2);

/// Per-unit weight of a resource from the catalog (grams). Defaults to a
/// conservative 1 kg when the catalog has no entry.
fn unit_weight_g(rid: ResourceId) -> u32 {
    core_ids::catalog()
        .get(rid)
        .map(|d| d.weight_g)
        .filter(|w| *w > 0)
        .unwrap_or(1000)
}

/// How many units of `rid` a vehicle with `payload_g` cargo can carry.
pub fn capacity_units(payload_g: u32, rid: ResourceId) -> u32 {
    (payload_g / unit_weight_g(rid)).max(1)
}

/// The cargo payload (grams) of a design — `derive_stats().max_payload_g`.
pub fn design_payload_g(design: &VehicleDesign, data: &VehicleData) -> u32 {
    derive_stats(&design.grid, data).max_payload_g
}

/// True when a design can be used for cargo hauling — a `Cargo`-purpose
/// design with a non-zero payload.
pub fn design_is_cargo_capable(design: &VehicleDesign, data: &VehicleData) -> bool {
    design.allowed_purpose == VehiclePurpose::Cargo && design_payload_g(design, data) > 0
}

/// Find one trained Cattle / Horse owned by `faction_id` not already claimed
/// and not in `taken` (claimed earlier this pass).
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

/// Sum of a blueprint's unmet deposit slots for `rid`.
fn blueprint_remaining_need(bp: &Blueprint, rid: ResourceId) -> u32 {
    let mut total = 0u32;
    for i in 0..bp.deposit_count as usize {
        if bp.deposits[i].resource_id == rid {
            total = total.saturating_add(
                bp.deposits[i].needed.saturating_sub(bp.deposits[i].deposited) as u32,
            );
        }
    }
    total
}

/// Live cargo weight (grams) of a vehicle's inventory.
fn cargo_weight_g(inv: &VehicleInventory) -> u32 {
    inv.items
        .iter()
        .map(|(rid, q)| unit_weight_g(*rid).saturating_mul(*q))
        .fold(0u32, |a, b| a.saturating_add(b))
}

/// Chebyshev distance between two tiles.
fn cheb(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// True iff `tile` is a road-grade surface (vehicles roll fastest here).
fn is_road_like(chunk_map: &ChunkMap, tile: (i32, i32)) -> bool {
    matches!(
        chunk_map.tile_kind_at(tile.0, tile.1),
        Some(TileKind::Road | TileKind::Bridge | TileKind::Dam)
    )
}

/// `VehiclePathFollow.cursor` starts here — `path[0]` is the start node.
const ROUTE_CURSOR_START: usize = 1;
/// Node budget for one `footprint_astar` call.
const VEHICLE_ROUTE_BUDGET: usize = 6_000;
/// Pixels-per-second a vehicle travels per unit of its terrain speed cap.
const VEHICLE_SPEED_PER_CAP: f32 = 40.0;
/// `vehicle_haul_recovery_system` cadence (ticks).
const VEHICLE_RECOVERY_CADENCE: u64 = 60;
/// Health lost per spanned Z-level when a vehicle overturns under its crew.
const ROLLOVER_FALL_DAMAGE_PER_Z: u8 = 8;

/// Plan a `footprint_astar` route for a vehicle from its current pose to a
/// tile adjacent to (or on) `target`. Tries `target` then its surrounding
/// rings as goal anchors; `cell_ok` folds `passable_at` + clearance +
/// occupancy (excluding the vehicle itself). Returns the node path, or `None`
/// if no candidate anchor is reachable.
#[allow(clippy::too_many_arguments)]
fn plan_vehicle_route(
    scratch: &mut VehiclePathScratch,
    design: &VehicleDesign,
    data: &VehicleData,
    self_e: Entity,
    from_anchor: (i32, i32),
    from_z: i8,
    from_heading: u8,
    target: (i32, i32),
    chunk_map: &ChunkMap,
    occupancy: &VehicleOccupancyIndex,
) -> Option<Vec<VehicleNode>> {
    let footprint = VehicleFootprint::from_grid(&design.grid);
    let height_z = footprint.height_z.max(1) as i32;
    let stats = derive_stats(&design.grid, data);
    let turn_cost = ((stats.turn_radius * 30.0) as u32).max(40);

    let cell_ok = |x: i32, y: i32, z: i32| -> bool {
        if !chunk_map.passable_at(x, y, z) {
            return false;
        }
        if chunk_map.vertical_clearance_at(x, y) < height_z {
            return false;
        }
        match occupancy.0.get(&(x, y)) {
            Some(&occ) => occ == self_e,
            None => true,
        }
    };

    let start = VehicleNode::new(from_anchor.0, from_anchor.1, from_z, from_heading);
    // Goal anchors: the target tile, then its surrounding rings (closest first).
    let mut goals: Vec<(i32, i32)> = vec![target];
    for r in 1..=2i32 {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) == r {
                    goals.push((target.0 + dx, target.1 + dy));
                }
            }
        }
    }
    for g in goals {
        match footprint_astar(
            scratch,
            &footprint.offsets_by_heading,
            start,
            g,
            turn_cost,
            cell_ok,
            VEHICLE_ROUTE_BUDGET,
        ) {
            VehiclePathResult::Found(path) if path.len() >= 2 => return Some(path),
            _ => {}
        }
    }
    None
}

/// True iff a vehicle of `design` parked at `anchor`/`heading` has a footprint
/// tile within chebyshev 1 of `target` — close enough to load / deliver.
fn footprint_reaches(
    design: &VehicleDesign,
    anchor: (i32, i32),
    heading: u8,
    target: (i32, i32),
) -> bool {
    footprint_tiles(design, anchor, heading)
        .iter()
        .any(|&t| cheb(t, target) <= 1)
}

/// ParallelB dispatcher. For each idle `JobClaim::Haul` holder whose posting
/// is bulky enough to amortise a vehicle, claims an idle owner-faction cargo
/// vehicle (+ trained draft animals) and routes the worker **on foot** to the
/// vehicle so the executor can board them. The vehicle itself moves later via
/// `footprint_astar` — this leg is plain single-tile person routing.
#[allow(clippy::too_many_arguments)]
pub fn htn_vehicle_haul_dispatch_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    board: Res<JobBoard>,
    registry: Res<VehicleDesignRegistry>,
    data: Res<VehicleData>,
    mut vehicles_q: Query<(Entity, &mut Vehicle, &VehicleInventory, &mut VehicleDraft)>,
    animals_q: Query<(Entity, &DomesticAnimal, &Tamed), Without<AnimalWorkClaim>>,
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
        (With<Person>, Without<Drafted>, Without<BoardedVehicle>),
    >,
) {
    let now = clock.tick as u32;
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
        if target.saturating_sub(delivered) < VEHICLE_HAUL_MIN_REMAINING {
            continue;
        }

        // Resume this worker's already-claimed vehicle (a prior walk-to-vehicle
        // leg that was preempted), or claim a fresh idle one.
        let resumed = vehicles_q
            .iter()
            .find(|(_, v, _, _)| v.hauler == Some(worker))
            .map(|(e, v, _, _)| (e, v.anchor_tile));

        let (vehicle_e, vehicle_tile) = if let Some(found) = resumed {
            found
        } else {
            let pick = vehicles_q.iter().find_map(|(e, v, _, _)| {
                if v.owner_faction != fm.faction_id
                    || v.hauler.is_some()
                    || v.state == VehicleState::Overturned
                {
                    return None;
                }
                let design = registry.get(v.design_id)?;
                if design_is_cargo_capable(design, &data) {
                    Some((e, v.anchor_tile, design.required_animals))
                } else {
                    None
                }
            });
            let Some((vehicle_e, vehicle_tile, required_animals)) = pick else {
                continue;
            };
            // Claim trained draft animals for a draft design.
            let mut hitched: Vec<Entity> = Vec::new();
            let mut ok = true;
            for _ in 0..required_animals {
                match pick_idle_draft_animal(fm.faction_id, &animals_q, &claimed_this_pass) {
                    Some(a) => {
                        claimed_this_pass.insert(a);
                        hitched.push(a);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            for &a in &hitched {
                commands.entity(a).insert(AnimalWorkClaim {
                    worker,
                    use_kind: AnimalUse::Cart,
                    expires_tick: now.saturating_add(VEHICLE_CLAIM_TTL_TICKS),
                });
            }
            if let Ok((_, mut v, _, mut draft)) = vehicles_q.get_mut(vehicle_e) {
                v.hauler = Some(worker);
                draft.hitched = hitched;
            }
            (vehicle_e, vehicle_tile)
        };

        // Route the worker on foot to the vehicle so they can board it.
        let worker_tile = world_to_tile(tr.translation.truncate());
        let cur_chunk = ChunkCoord(
            worker_tile.0.div_euclid(CHUNK_SIZE as i32),
            worker_tile.1.div_euclid(CHUNK_SIZE as i32),
        );
        let routed = assign_task_with_routing(
            &mut ai,
            worker_tile,
            cur_chunk,
            vehicle_tile,
            TaskKind::VehicleCargoHaul,
            None,
            &chunk_graph,
            &chunk_router,
            &chunk_map,
            &chunk_connectivity,
        );
        if !routed {
            continue;
        }
        let _ = aq.dispatch(Task::VehicleCargoHaul {
            vehicle: vehicle_e,
            blueprint,
            resource_id,
        });
    }
}

/// `repark_vehicle` for the executor's 4-tuple query — clears the haul state
/// (`hauler` / draft / in-flight route) so the vehicle re-pools as idle.
fn repark_helper(
    commands: &mut Commands,
    vehicles_q: &mut Query<(
        &mut Vehicle,
        &mut VehicleInventory,
        &mut VehicleDraft,
        Option<&VehiclePathFollow>,
    )>,
    vehicle_e: Entity,
) {
    if let Ok((mut v, _, mut draft, _)) = vehicles_q.get_mut(vehicle_e) {
        if v.state != VehicleState::Overturned {
            v.state = VehicleState::Parked;
        }
        v.hauler = None;
        draft.hitched.clear();
    }
    commands.entity(vehicle_e).remove::<VehiclePathFollow>();
}

/// Sequential executor for `Task::VehicleCargoHaul`. Boards the worker when
/// they reach the vehicle, then drives the two-phase load/deliver state
/// machine — planning the vehicle's `footprint_astar` route and resolving the
/// cargo transfer on arrival.
#[allow(clippy::too_many_arguments)]
pub fn vehicle_cargo_haul_task_system(
    mut commands: Commands,
    mut board: ResMut<JobBoard>,
    mut completed_events: EventWriter<JobCompletedEvent>,
    clock: Res<SimClock>,
    registry: Res<VehicleDesignRegistry>,
    data: Res<VehicleData>,
    chunk_map: Res<ChunkMap>,
    occupancy: Res<VehicleOccupancyIndex>,
    storage_tile_map: Res<StorageTileMap>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut bp_q: Query<&mut Blueprint>,
    mut vehicles_q: Query<(
        &mut Vehicle,
        &mut VehicleInventory,
        &mut VehicleDraft,
        Option<&VehiclePathFollow>,
    )>,
    mut workers: Query<
        (
            Entity,
            &mut PersonAI,
            &mut ActionQueue,
            &mut Transform,
            &BucketSlot,
            &LodLevel,
            &JobClaim,
            Option<&BoardedVehicle>,
        ),
        With<Person>,
    >,
    mut scratch: Local<VehiclePathScratch>,
) {
    for (worker, mut ai, mut aq, mut tr, slot, lod, claim, boarded) in workers.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if aq.current_task_kind() != TaskKind::VehicleCargoHaul as u16 {
            continue;
        }
        let Some((vehicle_e, blueprint, rid)) = aq.current.as_vehicle_cargo_haul() else {
            aq.cancel_chain(&mut ai);
            continue;
        };

        let release_draft = |commands: &mut Commands,
                             vq: &mut Query<(
            &mut Vehicle,
            &mut VehicleInventory,
            &mut VehicleDraft,
            Option<&VehiclePathFollow>,
        )>| {
            if let Ok((_, _, draft, _)) = vq.get(vehicle_e) {
                for &a in &draft.hitched {
                    release_animal_work_claim(commands, a);
                }
            }
        };

        // The vehicle vanished — abort cleanly.
        if vehicles_q.get(vehicle_e).is_err() {
            release_draft(&mut commands, &mut vehicles_q);
            commands.entity(worker).remove::<JobClaim>();
            if boarded.is_some() {
                commands.entity(worker).remove::<BoardedVehicle>();
            }
            aq.cancel_chain(&mut ai);
            continue;
        }
        // Claim revoked under us.
        if !matches!(claim.kind, JobKind::Haul) {
            release_draft(&mut commands, &mut vehicles_q);
            repark_helper(&mut commands, &mut vehicles_q, vehicle_e);
            if boarded.is_some() {
                commands.entity(worker).remove::<BoardedVehicle>();
            }
            aq.cancel_chain(&mut ai);
            continue;
        }

        // ── Boarding ────────────────────────────────────────────────────
        if boarded.is_none() {
            // Still walking to the vehicle until movement flips us to Working.
            if ai.state != AiState::Working {
                continue;
            }
            commands
                .entity(worker)
                .insert(BoardedVehicle { vehicle: vehicle_e });
            if let Ok((mut v, _, _, _)) = vehicles_q.get_mut(vehicle_e) {
                v.hauler = Some(worker);
                v.state = VehicleState::Moving;
            }
            // The route is planned on the next tick's boarded branch.
            continue;
        }

        // ── Boarded: drive the load/deliver state machine ────────────────
        let (anchor, heading, vz, has_path, loaded, design_id) = {
            let (v, inv, _, path) = vehicles_q.get(vehicle_e).unwrap();
            (
                v.anchor_tile,
                v.heading,
                v.z,
                path.is_some(),
                !inv.is_empty(),
                v.design_id,
            )
        };
        // Wait while the vehicle is still travelling.
        if has_path {
            continue;
        }
        let Some(design) = registry.get(design_id) else {
            release_draft(&mut commands, &mut vehicles_q);
            repark_helper(&mut commands, &mut vehicles_q, vehicle_e);
            commands.entity(worker).remove::<BoardedVehicle>();
            commands.entity(worker).remove::<JobClaim>();
            aq.cancel_chain(&mut ai);
            continue;
        };

        let abort = |commands: &mut Commands,
                     vq: &mut Query<(
            &mut Vehicle,
            &mut VehicleInventory,
            &mut VehicleDraft,
            Option<&VehiclePathFollow>,
        )>,
                     ai: &mut PersonAI,
                     aq: &mut ActionQueue| {
            if let Ok((_, _, draft, _)) = vq.get(vehicle_e) {
                for &a in &draft.hitched {
                    release_animal_work_claim(commands, a);
                }
            }
            repark_helper(commands, vq, vehicle_e);
            commands.entity(worker).remove::<BoardedVehicle>();
            commands.entity(worker).remove::<JobClaim>();
            aq.cancel_chain(ai);
        };

        if !loaded {
            // ── LOAD: drive to storage, then transfer cargo ──────────────
            let Some(src) = storage_tile_map
                .by_faction
                .get(&claim.faction_id)
                .and_then(|tiles| nearest_tile(anchor, tiles))
            else {
                abort(&mut commands, &mut vehicles_q, &mut ai, &mut aq);
                continue;
            };
            if !footprint_reaches(design, anchor, heading, src) {
                match plan_vehicle_route(
                    &mut scratch, design, &data, vehicle_e, anchor, vz, heading, src,
                    &chunk_map, &occupancy,
                ) {
                    Some(path) => {
                        commands.entity(vehicle_e).insert(VehiclePathFollow {
                            path,
                            cursor: ROUTE_CURSOR_START,
                            tip_torque: 0.0,
                        });
                    }
                    None => abort(&mut commands, &mut vehicles_q, &mut ai, &mut aq),
                }
                continue;
            }
            // At the storage tile — transfer cargo.
            let need = bp_q
                .get(blueprint)
                .map(|bp| blueprint_remaining_need(&bp, rid))
                .unwrap_or(0);
            if need == 0 {
                release_draft(&mut commands, &mut vehicles_q);
                repark_helper(&mut commands, &mut vehicles_q, vehicle_e);
                commands.entity(worker).remove::<BoardedVehicle>();
                commands.entity(worker).remove::<JobClaim>();
                aq.finish_task(&mut ai);
                continue;
            }
            let payload_g = design_payload_g(design, &data);
            let want = need.min(capacity_units(payload_g, rid));
            let mut loaded_qty = 0u32;
            for gi_e in spatial.get(src.0, src.1).to_vec() {
                if loaded_qty >= want {
                    break;
                }
                if let Ok(mut gi) = ground_items.get_mut(gi_e) {
                    if gi.item.resource_id != rid || gi.qty == 0 {
                        continue;
                    }
                    let take = (want - loaded_qty).min(gi.qty);
                    gi.qty -= take;
                    loaded_qty += take;
                    if gi.qty == 0 {
                        commands.entity(gi_e).despawn_recursive();
                    }
                }
            }
            if loaded_qty == 0 {
                abort(&mut commands, &mut vehicles_q, &mut ai, &mut aq);
                continue;
            }
            if let Ok((mut v, mut inv, _, _)) = vehicles_q.get_mut(vehicle_e) {
                inv.add(rid, loaded_qty);
                v.state = VehicleState::Moving;
            }
            // Next tick the boarded branch plans the deliver route.
        } else {
            // ── DELIVER: drive to the blueprint, then deposit ────────────
            let Ok(deliver_tile) = bp_q
                .get(blueprint)
                .map(|bp| bp.work_stand.unwrap_or(bp.tile))
            else {
                // Blueprint gone — spill the load at the vehicle, abort.
                let carried: Vec<(ResourceId, u32)> = vehicles_q
                    .get(vehicle_e)
                    .map(|(_, inv, _, _)| inv.items.clone())
                    .unwrap_or_default();
                for (r, q) in carried {
                    if q > 0 {
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands, &spatial, &mut ground_items, anchor.0, anchor.1, r, q,
                        );
                    }
                }
                if let Ok((_, mut inv, _, _)) = vehicles_q.get_mut(vehicle_e) {
                    inv.items.clear();
                }
                abort(&mut commands, &mut vehicles_q, &mut ai, &mut aq);
                continue;
            };
            if !footprint_reaches(design, anchor, heading, deliver_tile) {
                match plan_vehicle_route(
                    &mut scratch, design, &data, vehicle_e, anchor, vz, heading, deliver_tile,
                    &chunk_map, &occupancy,
                ) {
                    Some(path) => {
                        commands.entity(vehicle_e).insert(VehiclePathFollow {
                            path,
                            cursor: ROUTE_CURSOR_START,
                            tip_torque: 0.0,
                        });
                    }
                    None => abort(&mut commands, &mut vehicles_q, &mut ai, &mut aq),
                }
                continue;
            }
            // At the blueprint — deposit the load.
            let carried = vehicles_q
                .get(vehicle_e)
                .map(|(_, inv, _, _)| inv.qty_of(rid))
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
                    let still =
                        bp.deposits[i].needed.saturating_sub(bp.deposits[i].deposited) as u32;
                    let take = still.min(remaining).min(u8::MAX as u32);
                    bp.deposits[i].deposited =
                        bp.deposits[i].deposited.saturating_add(take as u8);
                    remaining -= take;
                    deposited += take;
                }
            }
            let residual = {
                let mut res = 0u32;
                if let Ok((_, mut inv, _, _)) = vehicles_q.get_mut(vehicle_e) {
                    inv.take(rid, carried);
                    res = carried.saturating_sub(deposited);
                }
                res
            };
            if residual > 0 {
                crate::simulation::items::spawn_or_merge_ground_item(
                    &mut commands, &spatial, &mut ground_items, anchor.0, anchor.1, rid, residual,
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
            let posting_done = board
                .get(claim.job_id)
                .map(|p| p.progress.is_complete())
                .unwrap_or(true);
            if posting_done {
                release_draft(&mut commands, &mut vehicles_q);
                repark_helper(&mut commands, &mut vehicles_q, vehicle_e);
                // Unboard: place the worker on the vehicle's tile, free them.
                commands.entity(worker).remove::<BoardedVehicle>();
                commands.entity(worker).remove::<JobClaim>();
                let wp = tile_to_world(anchor.0, anchor.1);
                tr.translation.x = wp.x;
                tr.translation.y = wp.y;
                ai.target_tile = anchor;
                ai.dest_tile = anchor;
                aq.finish_task(&mut ai);
            } else if let Ok((mut v, _, _, _)) = vehicles_q.get_mut(vehicle_e) {
                // More to haul — next tick plans another load run.
                v.state = VehicleState::Moving;
            }
        }
    }
}

/// Sequential (after `movement_system`): step every vehicle along its
/// `VehiclePathFollow`, updating `anchor_tile` / `heading` / `z` as it
/// completes each node and accumulating rollover tip-torque per step. The
/// component is removed when the route completes — the executor's
/// "vehicle arrived" signal.
#[allow(clippy::too_many_arguments)]
pub fn vehicle_movement_system(
    mut commands: Commands,
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    registry: Res<VehicleDesignRegistry>,
    data: Res<VehicleData>,
    lod_q: Query<&LodLevel>,
    mut vehicles: Query<(
        Entity,
        &mut Vehicle,
        &mut Transform,
        &VehicleInventory,
        &mut VehiclePathFollow,
    )>,
) {
    let dt = time.delta_secs() * clock.scale_factor();
    for (e, mut v, mut tf, inv, mut pf) in vehicles.iter_mut() {
        if v.state == VehicleState::Overturned {
            continue;
        }
        // A vehicle whose driver is Dormant holds position.
        if let Some(h) = v.hauler {
            if matches!(lod_q.get(h), Ok(LodLevel::Dormant)) {
                continue;
            }
        }
        if pf.cursor >= pf.path.len() {
            commands.entity(e).remove::<VehiclePathFollow>();
            v.state = VehicleState::Parked;
            continue;
        }
        let Some(design) = registry.get(v.design_id) else {
            commands.entity(e).remove::<VehiclePathFollow>();
            continue;
        };
        let stats = derive_stats(&design.grid, &data);
        let node = pf.path[pf.cursor];
        let goal = tile_to_world(node.x, node.y);
        let here = tf.translation.truncate();
        let delta = goal - here;
        let cap = if is_road_like(&chunk_map, v.anchor_tile) {
            stats.road_speed_cap
        } else {
            stats.offroad_speed_cap
        };
        let step = (cap.max(0.2) * VEHICLE_SPEED_PER_CAP * dt).max(0.5);
        if delta.length() <= step {
            // Reached the node — snap, commit pose, accumulate rollover.
            tf.translation.x = goal.x;
            tf.translation.y = goal.y;
            let prev = pf.path[pf.cursor.saturating_sub(1)];
            let turned = node.heading != prev.heading;
            let z_slope = (node.z as i32 - prev.z as i32).abs();
            let overloaded = cargo_weight_g(inv) > stats.max_payload_g;
            let ctx = RolloverContext {
                turn_sharpness: if turned {
                    (2.0 / stats.turn_radius.max(0.5)).min(2.0)
                } else {
                    0.0
                },
                z_slope,
                rough_terrain: matches!(
                    chunk_map.tile_kind_at(node.x, node.y),
                    Some(TileKind::Marsh | TileKind::Sand | TileKind::Scrub | TileKind::Snow)
                ),
                overloaded,
            };
            pf.tip_torque += step_tip_torque(&stats, &ctx);
            v.anchor_tile = (node.x, node.y);
            v.heading = node.heading;
            v.z = node.z;
            pf.cursor += 1;
        } else {
            let dir = delta / delta.length();
            tf.translation.x += dir.x * step;
            tf.translation.y += dir.y * step;
        }
    }
}

/// Sequential (after `vehicle_movement_system`): overturn a vehicle whose
/// accumulated tip-torque has beaten its `stability`. Ejects the crew (fall
/// damage scaled by height), spills the cargo, releases the draft animals.
#[allow(clippy::too_many_arguments)]
pub fn vehicle_rollover_system(
    mut commands: Commands,
    chunk_map: Res<ChunkMap>,
    registry: Res<VehicleDesignRegistry>,
    data: Res<VehicleData>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut vehicles: Query<(
        &mut Vehicle,
        &mut VehicleInventory,
        &mut VehicleDraft,
        &mut VehicleCrew,
        &VehiclePathFollow,
    )>,
    mut workers: Query<
        (&mut Transform, &mut PersonAI, &mut ActionQueue, &mut Health),
        (With<Person>, Without<Vehicle>),
    >,
) {
    for (mut v, mut inv, mut draft, mut crew, pf) in vehicles.iter_mut() {
        if v.state == VehicleState::Overturned {
            continue;
        }
        let Some(design) = registry.get(v.design_id) else {
            continue;
        };
        let stats = derive_stats(&design.grid, &data);
        if !vehicle_rolls_over(&stats, pf.tip_torque) {
            continue;
        }
        v.state = VehicleState::Overturned;
        let (ax, ay) = v.anchor_tile;

        // Eject the driver onto an adjacent passable tile, with fall damage.
        if let Some(hauler) = v.hauler.take() {
            if let Ok((mut tr, mut ai, mut aq, mut health)) = workers.get_mut(hauler) {
                let landing = (-1..=1)
                    .flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| (ax + dx, ay + dy))
                    .find(|&(tx, ty)| {
                        chunk_map.passable_at(tx, ty, chunk_map.surface_z_at(tx, ty))
                    })
                    .unwrap_or((ax, ay));
                let wp = tile_to_world(landing.0, landing.1);
                tr.translation.x = wp.x;
                tr.translation.y = wp.y;
                let fall = stats.height_z.saturating_mul(ROLLOVER_FALL_DAMAGE_PER_Z);
                health.current = health.current.saturating_sub(fall);
                ai.target_tile = landing;
                ai.dest_tile = landing;
                aq.cancel_chain(&mut ai);
            }
            commands.entity(hauler).remove::<BoardedVehicle>();
            commands.entity(hauler).remove::<JobClaim>();
        }

        // Spill the cargo at the overturn tile.
        for (rid, qty) in std::mem::take(&mut inv.items) {
            if qty > 0 {
                crate::simulation::items::spawn_or_merge_ground_item(
                    &mut commands, &spatial, &mut ground_items, ax, ay, rid, qty,
                );
            }
        }
        // Release the draft animals.
        for &a in &draft.hitched {
            release_animal_work_claim(&mut commands, a);
        }
        draft.hitched.clear();
        // Eject the seated combat crew — an overturned hulk carries no one.
        for rider in eject_all_crew(&mut crew) {
            commands.entity(rider).remove::<BoardedVehicle>();
        }
    }
}

/// Drain every crew slot (driver / passengers / gunners), returning the
/// ejected entities. The caller strips `BoardedVehicle` from each.
fn eject_all_crew(crew: &mut VehicleCrew) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    out.extend(crew.driver.take());
    out.extend(crew.passengers.drain(..));
    out.extend(crew.gunners.drain(..));
    out
}

/// Sequential (after `vehicle_rollover_system`): snap the boarded driver, the
/// seated combat crew (driver / passengers / gunners), and every hitched draft
/// animal to the vehicle's position each tick — the vehicle leads, the crew
/// ride it. A chariot is a *mobile crew platform*: its seated crew fight
/// through the normal `combat_system` rules from wherever the vehicle is.
pub fn vehicle_crew_sync_system(
    vehicles: Query<(&Vehicle, &VehicleDraft, &VehicleCrew, &Transform)>,
    mut crew: Query<&mut Transform, Without<Vehicle>>,
    mut crew_ai: Query<&mut PersonAI>,
) {
    for (v, draft, vcrew, vtf) in vehicles.iter() {
        // Every rider (cargo hauler + every combat-crew slot) snaps to the
        // vehicle. De-duped so an entity seated *and* hauling is only moved
        // once (idempotent regardless).
        let mut riders: Vec<Entity> = Vec::new();
        for r in v
            .hauler
            .into_iter()
            .chain(vcrew.driver)
            .chain(vcrew.passengers.iter().copied())
            .chain(vcrew.gunners.iter().copied())
        {
            if !riders.contains(&r) {
                riders.push(r);
            }
        }
        for rider in riders {
            if let Ok(mut tr) = crew.get_mut(rider) {
                tr.translation.x = vtf.translation.x;
                tr.translation.y = vtf.translation.y;
            }
            if let Ok(mut ai) = crew_ai.get_mut(rider) {
                ai.current_z = v.z;
            }
        }
        for (i, &animal) in draft.hitched.iter().enumerate() {
            if let Ok(mut tr) = crew.get_mut(animal) {
                tr.translation.x = vtf.translation.x + TILE_SIZE * (0.5 + i as f32 * 0.4);
                tr.translation.y = vtf.translation.y + TILE_SIZE * 0.6;
            }
        }
    }
}

/// Economy (cadence-gated): re-park a vehicle whose `hauler` has died or is no
/// longer running a `VehicleCargoHaul` — backstop for a driver lost mid-haul.
pub fn vehicle_haul_recovery_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    mut vehicles: Query<(Entity, &mut Vehicle, &VehicleInventory, &mut VehicleDraft)>,
    workers: Query<(&ActionQueue, Option<&BoardedVehicle>), With<Person>>,
) {
    if clock.tick % VEHICLE_RECOVERY_CADENCE != 0 {
        return;
    }
    for (e, mut v, _inv, mut draft) in vehicles.iter_mut() {
        let Some(hauler) = v.hauler else {
            continue;
        };
        let alive = match workers.get(hauler) {
            Ok((aq, boarded)) => {
                aq.current_task_kind() == TaskKind::VehicleCargoHaul as u16 || boarded.is_some()
            }
            Err(_) => false,
        };
        if alive {
            continue;
        }
        for &a in &draft.hitched {
            release_animal_work_claim(&mut commands, a);
        }
        draft.hitched.clear();
        v.hauler = None;
        if v.state != VehicleState::Overturned {
            v.state = VehicleState::Parked;
        }
        commands.entity(e).remove::<VehiclePathFollow>();
    }
}

// ── Phase 5: AI vehicle provisioning ──────────────────────────────────────
//
// AI factions don't build vehicle yards or queue vehicles by hand. Two daily
// Economy systems give a qualifying settled faction a cargo vehicle: an
// intent emitter drops a `VehicleYard` blueprint, and once the yard is built
// the auto-queue enqueues a conservative stock template (`Handcart`).

/// Minimum member count before a faction is worth a vehicle yard — below this
/// the haul volume doesn't amortise the yard's build cost.
const YARD_MIN_MEMBERS: u32 = 10;

/// Economy (daily): emit a `VehicleYard` blueprint for a settled faction with
/// `ANIMAL_HUSBANDRY`, enough members, and no yard (built or pending).
#[allow(clippy::too_many_arguments)]
pub fn vehicle_yard_intent_emitter_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    bp_q: Query<&Blueprint>,
    yards: Query<&VehicleYard>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let mut have_yard: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for y in yards.iter() {
        have_yard.insert(y.faction_id);
    }
    let pending_yard = |fid: u32| -> bool {
        bp_map.0.values().any(|&e| {
            bp_q
                .get(e)
                .map(|bp| {
                    bp.faction_id == fid
                        && bp.kind == crate::simulation::construction::BuildSiteKind::VehicleYard
                })
                .unwrap_or(false)
        })
    };

    for (&fid, faction) in registry.factions.iter() {
        if faction.parent_faction.is_some() || faction.member_count < YARD_MIN_MEMBERS {
            continue;
        }
        if faction.caps.home.is_mobile() || !faction.techs.has(ANIMAL_HUSBANDRY) {
            continue;
        }
        if have_yard.contains(&fid) || pending_yard(fid) {
            continue;
        }
        // Place near a settlement edge — a ring 8..14 from home on open ground.
        let home = faction.home_tile;
        let mut placement: Option<(i32, i32)> = None;
        'outer: for r in 8i32..=14 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    let t = (home.0 + dx, home.1 + dy);
                    let Some(k) = chunk_map.tile_kind_at(t.0, t.1) else {
                        continue;
                    };
                    if matches!(k, TileKind::Grass | TileKind::Scrub | TileKind::Cropland)
                        && bp_map.0.get(&t).is_none()
                    {
                        placement = Some(t);
                        break 'outer;
                    }
                }
            }
        }
        let Some(tile) = placement else {
            continue;
        };
        let z = chunk_map.surface_z_at(tile.0, tile.1) as i8;
        let bp = Blueprint::new(
            fid,
            None,
            crate::simulation::construction::BuildSiteKind::VehicleYard,
            tile,
            z,
        );
        let wp = tile_to_world(tile.0, tile.1);
        commands.spawn((
            bp,
            Transform::from_xyz(wp.x, wp.y, 0.2),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ));
    }
}

/// Economy (daily): for a settled faction that owns a built `VehicleYard` but
/// no `Vehicle` (and has none queued), enqueue a conservative stock template.
pub fn vehicle_ai_queue_system(
    clock: Res<SimClock>,
    mut queue: ResMut<VehicleAssemblyQueue>,
    registry: Res<VehicleDesignRegistry>,
    yards: Query<&VehicleYard>,
    vehicles: Query<&Vehicle>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let Some(handcart) = registry.by_name("Handcart").map(|d| d.id) else {
        return;
    };
    let mut yard_factions: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for y in yards.iter() {
        yard_factions.insert(y.faction_id);
    }
    for fid in yard_factions {
        if vehicles.iter().any(|v| v.owner_faction == fid) {
            continue;
        }
        if queue.entries.iter().any(|&(f, _)| f == fid) {
            continue;
        }
        queue.entries.push((fid, handcart));
    }
}

/// Cadence of the AI design-proposal pass — weekly.
const PROPOSAL_CADENCE_TICKS: u64 = 7 * TICKS_PER_DAY as u64;

/// Economy (weekly): generate one freeform *proposal* design per qualifying
/// faction into `VehicleDesignRegistry`. Proposals are **not** auto-queued —
/// AI factions keep auto-queuing stock templates via `vehicle_ai_queue_system`;
/// proposals exist for the player to browse, edit, and queue from the
/// `vehicle_designer` window. A faction only earns a proposal once it knows
/// `BRONZE_CASTING` — below that, a "freeform" all-wood design is identical to
/// a stock template, so there is nothing distinct to propose. The proposal is
/// a metal-reinforced variant of the tech-best stock cargo/war template
/// (wheels + axles upgraded to copper). Deterministic and idempotent: one
/// authored design per faction.
pub fn vehicle_ai_design_proposal_system(
    clock: Res<SimClock>,
    factions: Res<FactionRegistry>,
    mut designs: ResMut<VehicleDesignRegistry>,
    data: Res<VehicleData>,
) {
    if clock.tick % PROPOSAL_CADENCE_TICKS != 0 {
        return;
    }
    // The strongest metal in the vehicle catalog — `core.ron` ships copper /
    // iron; copper is the metalworking-era reinforcement.
    let Some(metal) = core_ids::catalog().id_of("copper") else {
        return;
    };
    // Factions that already own an authored design (proposal or player
    // custom) — one proposal each.
    let mut authored: ahash::AHashSet<u32> = ahash::AHashSet::default();
    for d in designs.iter() {
        if let Some(f) = d.author_faction {
            authored.insert(f);
        }
    }
    // Collect first — the registry can't be mutated while a `by_name` borrow
    // into it is live.
    let mut proposals: Vec<VehicleDesign> = Vec::new();
    for (&fid, faction) in factions.factions.iter() {
        if faction.parent_faction.is_some() || authored.contains(&fid) {
            continue;
        }
        if !faction.techs.has(ANIMAL_HUSBANDRY) || !faction.techs.has(BRONZE_CASTING) {
            continue;
        }
        let base_name = if faction.techs.has(WAR_CHARIOT) {
            "War Chariot"
        } else if faction.techs.has(OX_CART) {
            "Four-Wheel Wagon"
        } else {
            "Ox Cart"
        };
        let Some(base) = designs.by_name(base_name) else {
            continue;
        };
        let mut grid = base.grid.clone();
        for (_, cell) in grid.cells.iter_mut() {
            if matches!(cell.kind, VehiclePartKind::Wheel | VehiclePartKind::Axle) {
                cell.material = metal;
                cell.durability = cell_durability(metal, &data);
            }
        }
        let proposal = VehicleDesign {
            id: VehicleDesignId(0), // reassigned by `insert`
            name: format!("Reinforced {base_name}"),
            grid,
            allowed_purpose: base.allowed_purpose,
            required_animals: base.required_animals,
            tech_gates: base.tech_gates.clone(),
            author_faction: Some(fid),
            revision: 0,
        };
        if validate_design(&proposal, &data).is_ok() {
            proposals.push(proposal);
        }
    }
    for p in proposals {
        designs.insert(p);
    }
}

// ── Phase 5: player right-click vehicle orders ────────────────────────────
//
// The right-click menu (`ui/orders.rs`) emits a faction-level
// `PlayerCommand::VehicleOrder { vehicle, kind }`; `drain_player_command_events_system`
// queues it onto `PendingVehicleOps`. `vehicle_player_command_system`
// (Sequential, before `vehicle_movement_system`) applies it. Each order is a
// direct effect — no HTN worker task — so a player can micro-manage a vehicle
// the way Pack/Pitch micro-manages a camp.

/// One queued player vehicle order. `MoveTo` plans a `footprint_astar` route;
/// the rest are immediate state changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleOrderKind {
    /// Drive the (idle, upright) vehicle to a destination tile.
    MoveTo((i32, i32)),
    /// Right an overturned vehicle (`Overturned` → `Parked`).
    Right,
    /// Top the cargo bay up from the nearest faction storage tile.
    Load,
    /// Spill the whole cargo bay onto the vehicle's tile.
    Unload,
    /// Seat the nearest idle faction member as the driver.
    AssignCrew,
    /// Harness idle trained draft animals up to the design's requirement.
    Hitch,
    /// Release every hitched draft animal.
    Unhitch,
    /// Salvage the vehicle — refund half the design bill, despawn it.
    Deconstruct,
}

/// Pending player vehicle orders, drained by `vehicle_player_command_system`.
#[derive(Resource, Default)]
pub struct PendingVehicleOps {
    pub ops: Vec<(Entity, VehicleOrderKind)>,
}

/// Fraction of the design bill refunded when a vehicle is salvaged.
const SALVAGE_REFUND_NUM: u32 = 1;
const SALVAGE_REFUND_DEN: u32 = 2;

/// Sequential (before `vehicle_movement_system`): apply queued player vehicle
/// orders. Each order acts on one `Vehicle` entity directly.
#[allow(clippy::too_many_arguments)]
pub fn vehicle_player_command_system(
    mut commands: Commands,
    mut pending: ResMut<PendingVehicleOps>,
    clock: Res<SimClock>,
    registry: Res<VehicleDesignRegistry>,
    data: Res<VehicleData>,
    chunk_map: Res<ChunkMap>,
    occupancy: Res<VehicleOccupancyIndex>,
    storage_tile_map: Res<StorageTileMap>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    animals_q: Query<(Entity, &DomesticAnimal, &Tamed), Without<AnimalWorkClaim>>,
    members_q: Query<(Entity, &FactionMember), (With<Person>, Without<Drafted>, Without<BoardedVehicle>)>,
    mut vehicles_q: Query<(
        &mut Vehicle,
        &mut VehicleInventory,
        &mut VehicleCrew,
        &mut VehicleDraft,
    )>,
    mut scratch: Local<VehiclePathScratch>,
) {
    if pending.ops.is_empty() {
        return;
    }
    let now = clock.tick as u32;
    let mut claimed_this_pass: ahash::AHashSet<Entity> = ahash::AHashSet::default();
    let mut crew_claimed: ahash::AHashSet<Entity> = ahash::AHashSet::default();

    for (vehicle_e, kind) in std::mem::take(&mut pending.ops) {
        let Ok((mut v, mut inv, mut crew, mut draft)) = vehicles_q.get_mut(vehicle_e) else {
            continue; // vehicle despawned before the order applied
        };
        let Some(design) = registry.get(v.design_id).cloned() else {
            continue;
        };
        let anchor = v.anchor_tile;

        match kind {
            VehicleOrderKind::MoveTo(dest) => {
                // Only an idle, upright vehicle obeys a player move — a
                // mid-haul vehicle is owned by the cargo executor.
                if v.hauler.is_some() || v.state == VehicleState::Overturned {
                    continue;
                }
                if let Some(path) = plan_vehicle_route(
                    &mut scratch,
                    &design,
                    &data,
                    vehicle_e,
                    anchor,
                    v.z,
                    v.heading,
                    dest,
                    &chunk_map,
                    &occupancy,
                ) {
                    v.state = VehicleState::Moving;
                    commands.entity(vehicle_e).insert(VehiclePathFollow {
                        path,
                        cursor: ROUTE_CURSOR_START,
                        tip_torque: 0.0,
                    });
                }
            }
            VehicleOrderKind::Right => {
                if v.state == VehicleState::Overturned {
                    v.state = VehicleState::Parked;
                    commands.entity(vehicle_e).remove::<VehiclePathFollow>();
                }
            }
            VehicleOrderKind::Load => {
                let payload_g = design_payload_g(&design, &data);
                let mut room_g = payload_g.saturating_sub(cargo_weight_g(&inv));
                if room_g == 0 {
                    continue;
                }
                let Some(src) = storage_tile_map
                    .by_faction
                    .get(&v.owner_faction)
                    .and_then(|tiles| nearest_tile(anchor, tiles))
                else {
                    continue;
                };
                for gi_e in spatial.get(src.0, src.1).to_vec() {
                    if room_g == 0 {
                        break;
                    }
                    if let Ok(mut gi) = ground_items.get_mut(gi_e) {
                        if gi.qty == 0 {
                            continue;
                        }
                        let rid = gi.item.resource_id;
                        let unit = unit_weight_g(rid);
                        let take = (room_g / unit).min(gi.qty);
                        if take == 0 {
                            continue;
                        }
                        gi.qty -= take;
                        inv.add(rid, take);
                        room_g = room_g.saturating_sub(take.saturating_mul(unit));
                        if gi.qty == 0 {
                            commands.entity(gi_e).despawn_recursive();
                        }
                    }
                }
            }
            VehicleOrderKind::Unload => {
                for (rid, qty) in std::mem::take(&mut inv.items) {
                    if qty > 0 {
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands,
                            &spatial,
                            &mut ground_items,
                            anchor.0,
                            anchor.1,
                            rid,
                            qty,
                        );
                    }
                }
            }
            VehicleOrderKind::AssignCrew => {
                // Seat idle faction members onto the vehicle: the first fills
                // the driver slot, the rest become passengers up to the crew
                // capacity (count of `CrewSeat` cells; a Cargo design with no
                // seats still gets a single driver). Each boards via
                // `BoardedVehicle` so `vehicle_crew_sync_system` rides them
                // and `combat_system` fights through them.
                let capacity = (crew_seat_count(&design) as usize).max(1);
                let mut seated = crew.driver.iter().count() + crew.passengers.len();
                for (person, fm) in members_q.iter() {
                    if seated >= capacity {
                        break;
                    }
                    if fm.faction_id != v.owner_faction || crew_claimed.contains(&person) {
                        continue;
                    }
                    crew_claimed.insert(person);
                    if crew.driver.is_none() {
                        crew.driver = Some(person);
                    } else {
                        crew.passengers.push(person);
                    }
                    commands
                        .entity(person)
                        .insert(BoardedVehicle { vehicle: vehicle_e });
                    seated += 1;
                }
            }
            VehicleOrderKind::Hitch => {
                let want = design.required_animals as usize;
                while draft.hitched.len() < want {
                    match pick_idle_draft_animal(v.owner_faction, &animals_q, &claimed_this_pass) {
                        Some(a) => {
                            claimed_this_pass.insert(a);
                            draft.hitched.push(a);
                            commands.entity(a).insert(AnimalWorkClaim {
                                worker: vehicle_e,
                                use_kind: AnimalUse::Cart,
                                expires_tick: now.saturating_add(VEHICLE_CLAIM_TTL_TICKS),
                            });
                        }
                        None => break,
                    }
                }
            }
            VehicleOrderKind::Unhitch => {
                for &a in &draft.hitched {
                    release_animal_work_claim(&mut commands, a);
                }
                draft.hitched.clear();
            }
            VehicleOrderKind::Deconstruct => {
                // Refund half the design bill onto the vehicle's tile.
                for (rid, qty) in design_bill(&design) {
                    let refund = qty.saturating_mul(SALVAGE_REFUND_NUM) / SALVAGE_REFUND_DEN;
                    if refund > 0 {
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands,
                            &spatial,
                            &mut ground_items,
                            anchor.0,
                            anchor.1,
                            rid,
                            refund,
                        );
                    }
                }
                // Spill any carried cargo too.
                for (rid, qty) in std::mem::take(&mut inv.items) {
                    if qty > 0 {
                        crate::simulation::items::spawn_or_merge_ground_item(
                            &mut commands,
                            &spatial,
                            &mut ground_items,
                            anchor.0,
                            anchor.1,
                            rid,
                            qty,
                        );
                    }
                }
                for &a in &draft.hitched {
                    release_animal_work_claim(&mut commands, a);
                }
                for rider in eject_all_crew(&mut crew) {
                    commands.entity(rider).remove::<BoardedVehicle>();
                }
                commands.entity(vehicle_e).despawn_recursive();
            }
        }
    }
}

// ── Phase 6: chariot crew, draft & combat ─────────────────────────────────
//
// A `Vehicle` is a destructible multi-cell body. Attacks aimed at the vehicle
// resolve against individual `VehicleHealth` cells via a height-aware
// hit-location roll: melee biases the low cells (wheels / axles), and
// destroying a cell drives a concrete failure —
//
// - **Wheel / Axle** → movement-disabled (`VehicleDisableFlags::MOVEMENT`);
//   the route is dropped and the vehicle parks. The intended ancient
//   counterplay: cripple the wheels and the chariot is out of the fight.
// - **CargoBay** → the load spills as `GroundItem`s.
// - **Hitch / Yoke** → the draft team is released.
// - **CrewSeat** → an occupant is ejected (exposed).
// - A destroyed low support cell (Frame / Deck / Axle on the bottom Z) drops
//   the chassis and overturns the vehicle outright.
// - When the last cell falls the vehicle is **wrecked** — cargo + animals +
//   crew are released and the hulk despawns.
//
// Crew offence needs no new code: a seated crew member rides the vehicle
// (`vehicle_crew_sync_system`) and fights any adjacent foe through the normal
// `combat_system` rules — a chariot is just a mobile crew platform.

/// What a resolved hit on a vehicle cell triggered. The combat system reads
/// this to apply the world-side effects (spilling cargo, ejecting crew, …).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VehicleHitOutcome {
    pub cell_destroyed: bool,
    pub movement_disabled: bool,
    pub spill_cargo: bool,
    pub release_animals: bool,
    pub eject_crew: bool,
    pub force_overturn: bool,
}

/// Pick a target cell for a melee hit. Among the still-standing cells the roll
/// is **height-weighted toward the low cells** — a foot soldier's spear, axe
/// or club reaches the wheels and axles far more readily than a high crew
/// platform. Returns `None` only when every cell is already destroyed.
pub fn pick_hit_cell(health: &VehicleHealth) -> Option<IVec3> {
    let live: Vec<IVec3> = health
        .cells
        .iter()
        .filter(|(_, h)| *h > 0)
        .map(|(p, _)| *p)
        .collect();
    if live.is_empty() {
        return None;
    }
    let max_z = live.iter().map(|p| p.z).max().unwrap();
    // Weight = (max_z - z + 1): the bottom row outweighs the top.
    let weights: Vec<u32> = live.iter().map(|p| (max_z - p.z + 1) as u32).collect();
    let total: u32 = weights.iter().sum();
    let mut roll = fastrand::u32(0..total.max(1));
    for (i, &w) in weights.iter().enumerate() {
        if roll < w {
            return Some(live[i]);
        }
        roll -= w;
    }
    live.last().copied()
}

/// Apply `dmg` to the cell at `pos`. On destruction, sets the matching
/// `VehicleHealth.disabled` bit and returns the world-side effects to apply.
pub fn apply_vehicle_cell_damage(
    health: &mut VehicleHealth,
    design: &VehicleDesign,
    pos: IVec3,
    dmg: u16,
) -> VehicleHitOutcome {
    let mut out = VehicleHitOutcome::default();
    let Some(slot) = health.cells.iter_mut().find(|(p, _)| *p == pos) else {
        return out;
    };
    if slot.1 == 0 {
        return out;
    }
    slot.1 = slot.1.saturating_sub(dmg);
    if slot.1 > 0 {
        return out;
    }
    out.cell_destroyed = true;
    let kind = design.grid.get(pos).map(|c| c.kind);
    let min_z = design.grid.bounds().map(|(lo, _)| lo.z).unwrap_or(0);
    match kind {
        Some(VehiclePartKind::Wheel | VehiclePartKind::Axle | VehiclePartKind::Track) => {
            health.disabled.set(VehicleDisableFlags::MOVEMENT);
            out.movement_disabled = true;
        }
        Some(VehiclePartKind::CargoBay) => {
            health.disabled.set(VehicleDisableFlags::CARGO);
            out.spill_cargo = true;
        }
        Some(VehiclePartKind::Hitch | VehiclePartKind::Yoke) => {
            out.release_animals = true;
        }
        Some(VehiclePartKind::CrewSeat) => {
            out.eject_crew = true;
        }
        _ => {}
    }
    // A destroyed low support cell drops the chassis — force a rollover.
    if pos.z == min_z
        && matches!(
            kind,
            Some(VehiclePartKind::Frame | VehiclePartKind::Deck | VehiclePartKind::Axle)
        )
    {
        out.force_overturn = true;
    }
    out
}

/// Melee cooldown between an attacker's swings at a vehicle (seconds) — mirrors
/// `combat::BASE_ATTACK_COOLDOWN`.
const VEHICLE_ATTACK_COOLDOWN: f32 = 1.0;
/// Base melee damage a bare-handed attacker deals to a vehicle cell.
const VEHICLE_BASE_ATTACK_DAMAGE: u16 = 2;

/// Sequential (after `combat_system`): resolve melee attacks aimed at a
/// `Vehicle`. `combat_system` recognises a vehicle target as a live entity but
/// can't damage it (a vehicle has no `Health` / `Body`); this system does the
/// per-cell hit-location resolution and applies the structural consequences.
#[allow(clippy::too_many_arguments)]
pub fn vehicle_combat_system(
    mut commands: Commands,
    registry: Res<VehicleDesignRegistry>,
    spatial: Res<SpatialIndex>,
    mut ground_items: Query<&mut GroundItem>,
    mut attackers: Query<(
        &CombatTarget,
        &Transform,
        &LodLevel,
        Option<&mut CombatCooldown>,
        Option<&Stats>,
        Option<&Equipment>,
    )>,
    mut vehicles: Query<(
        &mut Vehicle,
        &mut VehicleHealth,
        &mut VehicleInventory,
        &mut VehicleDraft,
        &mut VehicleCrew,
    )>,
) {
    // Vehicles wrecked / despawned earlier this pass — skip further hits.
    let mut wrecked: ahash::AHashSet<Entity> = ahash::AHashSet::default();

    for (target, tf, lod, mut cd, stats, eq) in attackers.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let Some(target_e) = target.0 else {
            continue;
        };
        if wrecked.contains(&target_e) {
            continue;
        }
        // Melee cooldown — `combat_system` already decremented it this tick.
        if let Some(ref cd) = cd {
            if cd.0 > 0.0 {
                continue;
            }
        }
        let Ok((mut v, mut health, mut inv, mut draft, mut crew)) = vehicles.get_mut(target_e)
        else {
            continue;
        };
        let Some(design) = registry.get(v.design_id).cloned() else {
            continue;
        };

        // Adjacency — the attacker must be within chebyshev 1 of a footprint
        // tile (the vehicle is not in `SpatialIndex`, so combat_system can't
        // resolve this itself).
        let ax = (tf.translation.x / TILE_SIZE).floor() as i32;
        let ay = (tf.translation.y / TILE_SIZE).floor() as i32;
        let fp = footprint_tiles(&design, v.anchor_tile, v.heading);
        if !fp.iter().any(|&(tx, ty)| cheb((ax, ay), (tx, ty)) <= 1) {
            continue;
        }

        // Damage — base + weapon bonus + positive STR modifier (mirrors
        // `combat_system`'s melee maths).
        let mut dmg = VEHICLE_BASE_ATTACK_DAMAGE;
        if let Some(eq) = eq {
            if let Some(weapon) = eq.items.get(&EquipmentSlot::MainHand) {
                if let Some(w) = weapon.weapon_stats {
                    dmg = dmg.saturating_add(w.damage_bonus as u16);
                }
            }
        }
        if let Some(s) = stats {
            dmg = dmg.saturating_add(stats::modifier(s.strength).max(0) as u16);
        }
        if let Some(ref mut cd) = cd {
            cd.0 = VEHICLE_ATTACK_COOLDOWN;
        }

        let Some(hit) = pick_hit_cell(&health) else {
            continue;
        };
        let outcome = apply_vehicle_cell_damage(&mut health, &design, hit, dmg);
        if !outcome.cell_destroyed {
            continue;
        }
        let (vx, vy) = v.anchor_tile;

        if outcome.movement_disabled {
            commands.entity(target_e).remove::<VehiclePathFollow>();
            if v.state == VehicleState::Moving {
                v.state = VehicleState::Parked;
            }
        }
        if outcome.spill_cargo {
            for (rid, qty) in std::mem::take(&mut inv.items) {
                if qty > 0 {
                    crate::simulation::items::spawn_or_merge_ground_item(
                        &mut commands,
                        &spatial,
                        &mut ground_items,
                        vx,
                        vy,
                        rid,
                        qty,
                    );
                }
            }
        }
        if outcome.release_animals {
            for &a in &draft.hitched {
                release_animal_work_claim(&mut commands, a);
            }
            draft.hitched.clear();
        }
        if outcome.eject_crew {
            // Expose one occupant — passenger / gunner first, driver last.
            let exposed = crew
                .passengers
                .pop()
                .or_else(|| crew.gunners.pop())
                .or_else(|| crew.driver.take());
            if let Some(rider) = exposed {
                commands.entity(rider).remove::<BoardedVehicle>();
            }
        }

        // A destroyed low support cell, or losing the last cell, overturns /
        // wrecks the vehicle: release cargo + animals + crew.
        let wreck = health.intact_cells() == 0;
        if outcome.force_overturn || wreck {
            if v.state != VehicleState::Overturned {
                v.state = VehicleState::Overturned;
            }
            commands.entity(target_e).remove::<VehiclePathFollow>();
            for (rid, qty) in std::mem::take(&mut inv.items) {
                if qty > 0 {
                    crate::simulation::items::spawn_or_merge_ground_item(
                        &mut commands,
                        &spatial,
                        &mut ground_items,
                        vx,
                        vy,
                        rid,
                        qty,
                    );
                }
            }
            for &a in &draft.hitched {
                release_animal_work_claim(&mut commands, a);
            }
            draft.hitched.clear();
            if let Some(h) = v.hauler.take() {
                commands.entity(h).remove::<BoardedVehicle>();
            }
            for rider in eject_all_crew(&mut crew) {
                commands.entity(rider).remove::<BoardedVehicle>();
            }
        }
        if wreck {
            wrecked.insert(target_e);
            commands.entity(target_e).despawn_recursive();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> VehicleData {
        load_vehicle_assets().0
    }

    fn cell(kind: VehiclePartKind, material: ResourceId) -> VehicleCell {
        VehicleCell {
            kind,
            material,
            durability: 100,
        }
    }

    /// Build a 1-tall rectangular pad of Frame cells plus the given extra
    /// cells — a quick fixture base.
    fn grid_from(cells: &[(i32, i32, i32, VehiclePartKind)]) -> VehicleGrid {
        let wood = core_ids::wood();
        VehicleGrid {
            cells: cells
                .iter()
                .map(|&(x, y, z, k)| (IVec3::new(x, y, z), cell(k, wood)))
                .collect(),
        }
    }

    #[test]
    fn stock_templates_all_validate() {
        let data = data();
        let (_, registry) = load_vehicle_assets();
        assert!(registry.len() >= 5, "expected 5 stock templates");
        for d in registry.iter() {
            assert!(
                validate_design(d, &data).is_ok(),
                "stock template {:?} failed validation: {:?}",
                d.name,
                validate_design(d, &data)
            );
        }
    }

    #[test]
    fn rejects_disconnected_body() {
        let data = data();
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 1, 0, VehiclePartKind::Hitch),
            // island, two tiles away.
            (3, 3, 0, VehiclePartKind::Frame),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err.contains(&DesignError::Disconnected));
    }

    #[test]
    fn rejects_floating_cell() {
        let data = data();
        // Two stacked cells with a gap: z0 frame, z2 frame (z1 missing).
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 0, 1, VehiclePartKind::Frame),
            (0, 0, 3, VehiclePartKind::Hitch),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err
            .iter()
            .any(|e| matches!(e, DesignError::FloatingCell(_))));
    }

    #[test]
    fn rejects_unsupported_wheel() {
        let data = data();
        // Wheel with no adjacent axle.
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Wheel),
            (1, 0, 0, VehiclePartKind::Frame),
            (2, 0, 0, VehiclePartKind::Hitch),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err
            .iter()
            .any(|e| matches!(e, DesignError::UnsupportedWheel(_))));
    }

    #[test]
    fn rejects_missing_driver() {
        let data = data();
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (1, 0, 0, VehiclePartKind::Frame),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err.contains(&DesignError::NoControlCell));
    }

    #[test]
    fn rejects_bad_hitch() {
        let data = data();
        // Needs 2 animals but only a single Hitch (capacity 1).
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 1, 0, VehiclePartKind::Hitch),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 2, &data).unwrap_err();
        assert!(err.contains(&DesignError::BadHitch));
    }

    #[test]
    fn rejects_blocked_cargo() {
        let data = data();
        // A 3x3 frame ring around a single enclosed CargoBay — the bay is not
        // on the bbox edge and touches only frame... actually frame is "open",
        // so build a fully enclosed bay surrounded by Wall cells instead.
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Wall),
            (1, 0, 0, VehiclePartKind::Wall),
            (2, 0, 0, VehiclePartKind::Wall),
            (0, 1, 0, VehiclePartKind::Wall),
            (1, 1, 0, VehiclePartKind::CargoBay),
            (2, 1, 0, VehiclePartKind::Wall),
            (0, 2, 0, VehiclePartKind::Wall),
            (1, 2, 0, VehiclePartKind::Hitch),
            (2, 2, 0, VehiclePartKind::Wall),
        ]);
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err
            .iter()
            .any(|e| matches!(e, DesignError::BlockedCargo(_))));
    }

    #[test]
    fn rejects_overloaded_axle() {
        let data = data();
        // Heavy stone wall slab (walls add no structural support) on one weak
        // skin axle/wheel — the chassis can't even carry itself.
        let skin = core_ids::skin();
        let stone = core_ids::stone();
        let g = VehicleGrid {
            cells: vec![
                (IVec3::new(0, 0, 0), cell(VehiclePartKind::Wheel, skin)),
                (IVec3::new(0, 1, 0), cell(VehiclePartKind::Axle, skin)),
                (IVec3::new(0, 2, 0), cell(VehiclePartKind::Wall, stone)),
                (IVec3::new(1, 2, 0), cell(VehiclePartKind::Wall, stone)),
                (IVec3::new(2, 2, 0), cell(VehiclePartKind::Wall, stone)),
                (IVec3::new(0, 3, 0), cell(VehiclePartKind::Hitch, skin)),
            ],
        };
        let err = validate_grid(&g, VehiclePurpose::Cargo, 0, &data).unwrap_err();
        assert!(err.contains(&DesignError::OverloadedAxle));
    }

    #[test]
    fn chariot_rule_requires_crew() {
        let data = data();
        let g = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 1, 0, VehiclePartKind::Hitch),
        ]);
        let err = validate_grid(&g, VehiclePurpose::War, 1, &data).unwrap_err();
        assert!(err.contains(&DesignError::ChariotRule));
    }

    #[test]
    fn material_swap_moves_mass_and_speed() {
        let data = data();
        let wood = core_ids::wood();
        let iron = core_ids::iron();
        let base = |wheel_mat: ResourceId| VehicleGrid {
            cells: vec![
                (IVec3::new(0, 0, 0), cell(VehiclePartKind::Wheel, wheel_mat)),
                (IVec3::new(1, 0, 0), cell(VehiclePartKind::Wheel, wheel_mat)),
                (IVec3::new(0, 1, 0), cell(VehiclePartKind::Axle, wood)),
                (IVec3::new(1, 1, 0), cell(VehiclePartKind::Axle, wood)),
                (IVec3::new(0, 2, 0), cell(VehiclePartKind::CargoBay, wood)),
                (IVec3::new(1, 2, 0), cell(VehiclePartKind::Hitch, wood)),
            ],
        };
        let s_wood = derive_stats(&base(wood), &data);
        let s_iron = derive_stats(&base(iron), &data);
        assert!(
            s_iron.empty_mass_g > s_wood.empty_mass_g,
            "iron wheels are denser → heavier"
        );
        assert!(
            s_iron.road_speed_cap > s_wood.road_speed_cap,
            "iron wheels have higher traction → faster"
        );
        assert!(
            s_iron.stress_margin != s_wood.stress_margin,
            "material swap changes structural margin"
        );
    }

    #[test]
    fn tall_narrow_less_stable_than_wide_low() {
        let data = data();
        // Tall narrow: a 1x1 column 4 tall.
        let tall = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 0, 1, VehiclePartKind::Frame),
            (0, 0, 2, VehiclePartKind::Frame),
            (0, 0, 3, VehiclePartKind::CrewSeat),
        ]);
        // Wide low: a 6-wide 1-tall pad.
        let wide = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (1, 0, 0, VehiclePartKind::Frame),
            (2, 0, 0, VehiclePartKind::Frame),
            (3, 0, 0, VehiclePartKind::Frame),
            (4, 0, 0, VehiclePartKind::Frame),
            (5, 0, 0, VehiclePartKind::CrewSeat),
        ]);
        let s_tall = derive_stats(&tall, &data);
        let s_wide = derive_stats(&wide, &data);
        assert!(s_tall.height_z == 4);
        assert!(s_wide.height_z == 1);
        assert!(
            s_wide.stability > s_tall.stability * 4.0,
            "wide low ({}) should be far more stable than tall narrow ({})",
            s_wide.stability,
            s_tall.stability
        );
    }

    #[test]
    fn footprint_rotation_maps_all_headings() {
        let (_, registry) = load_vehicle_assets();
        let wagon = registry.by_name("Four-Wheel Wagon").unwrap();
        let fp = VehicleFootprint::from_grid(&wagon.grid);
        let n = fp.offsets_by_heading[0].len();
        // Same occupied-cell count at every heading.
        for h in 0..4 {
            assert_eq!(fp.offsets_by_heading[h].len(), n);
            // Every offset re-anchored non-negative.
            for o in &fp.offsets_by_heading[h] {
                assert!(o.x >= 0 && o.y >= 0);
            }
        }
        // 90° rotation actually changes the shape (wagon is 3 wide x 4 deep).
        assert_ne!(fp.offsets_by_heading[0], fp.offsets_by_heading[1]);
    }

    #[test]
    fn wagon_carries_more_than_handcart() {
        let data = data();
        let (_, registry) = load_vehicle_assets();
        let handcart = registry.by_name("Handcart").unwrap();
        let wagon = registry.by_name("Four-Wheel Wagon").unwrap();
        let s_hc = derive_stats(&handcart.grid, &data);
        let s_wagon = derive_stats(&wagon.grid, &data);
        assert!(
            s_wagon.max_payload_g > s_hc.max_payload_g,
            "wagon payload {} should exceed handcart payload {}",
            s_wagon.max_payload_g,
            s_hc.max_payload_g
        );
    }

    #[test]
    fn design_bill_counts_cells_and_tools() {
        let (_, registry) = load_vehicle_assets();
        let handcart = registry.by_name("Handcart").unwrap();
        let bill = design_bill(handcart);
        let wood = core_ids::wood();
        let tools = core_ids::tools();
        let wood_qty = bill.iter().find(|(r, _)| *r == wood).map(|(_, q)| *q);
        let tool_qty = bill.iter().find(|(r, _)| *r == tools).map(|(_, q)| *q);
        // Handcart: 7 wood cells, 2 wheels + 2 axles → 4 tools.
        assert_eq!(wood_qty, Some(7));
        assert_eq!(tool_qty, Some(4));
    }

    #[test]
    fn war_chariot_bill_includes_copper() {
        let (_, registry) = load_vehicle_assets();
        let war = registry.by_name("War Chariot").unwrap();
        let bill = design_bill(war);
        let copper = core_ids::copper();
        // Two copper axles → 2 copper in the bill.
        assert_eq!(
            bill.iter().find(|(r, _)| *r == copper).map(|(_, q)| *q),
            Some(2)
        );
    }

    #[test]
    fn footprint_tiles_track_anchor_and_heading() {
        let (_, registry) = load_vehicle_assets();
        let wagon = registry.by_name("Four-Wheel Wagon").unwrap();
        let h0 = footprint_tiles(wagon, (10, 10), 0);
        let h1 = footprint_tiles(wagon, (10, 10), 1);
        assert_eq!(h0.len(), h1.len(), "rotation preserves the tile count");
        // Every tile is offset from the anchor.
        assert!(h0.iter().all(|&(x, y)| x >= 10 && y >= 10));
        // A 3-wide × 4-deep wagon footprint changes shape under rotation.
        assert_ne!(h0, h1);
    }

    #[test]
    fn wide_cart_on_road_does_not_roll() {
        let data = data();
        let (_, registry) = load_vehicle_assets();
        let oxcart = registry.by_name("Ox Cart").unwrap();
        let stats = derive_stats(&oxcart.grid, &data);
        // Loaded, on a road: a gentle in-radius turn, no slope, no rough
        // ground — but at its rated load.
        let ctx = RolloverContext {
            turn_sharpness: 0.6,
            z_slope: 0,
            rough_terrain: false,
            overloaded: false,
        };
        let torque = step_tip_torque(&stats, &ctx);
        assert!(
            !vehicle_rolls_over(&stats, torque),
            "a road-bound ox cart ({torque} vs stability {}) must not tip",
            stats.stability
        );
    }

    #[test]
    fn tall_narrow_overloaded_on_slope_rolls() {
        let data = data();
        // A 1×1 column 4 cells tall — high COM, minimal track width.
        let tall = grid_from(&[
            (0, 0, 0, VehiclePartKind::Frame),
            (0, 0, 1, VehiclePartKind::Frame),
            (0, 0, 2, VehiclePartKind::Frame),
            (0, 0, 3, VehiclePartKind::CrewSeat),
        ]);
        let stats = derive_stats(&tall, &data);
        let ctx = RolloverContext {
            turn_sharpness: 1.5,
            z_slope: 1,
            rough_terrain: true,
            overloaded: true,
        };
        let torque = step_tip_torque(&stats, &ctx);
        assert!(
            vehicle_rolls_over(&stats, torque),
            "a tall narrow overloaded vehicle on a slope ({torque} vs \
             stability {}) must overturn",
            stats.stability
        );
    }

    #[test]
    fn capacity_units_is_positive() {
        let data = data();
        let (_, registry) = load_vehicle_assets();
        let wagon = registry.by_name("Four-Wheel Wagon").unwrap();
        let payload = design_payload_g(wagon, &data);
        assert!(payload > 0, "a cargo wagon must have a non-zero payload");
        assert!(capacity_units(payload, core_ids::wood()) >= 1);
        assert!(capacity_units(payload, core_ids::stone()) >= 1);
        assert!(design_is_cargo_capable(wagon, &data));
        let chariot = registry.by_name("War Chariot").unwrap();
        assert!(
            !design_is_cargo_capable(chariot, &data),
            "a War-purpose chariot is not a cargo hauler"
        );
    }

    #[test]
    fn inventory_add_take_roundtrip() {
        let mut inv = VehicleInventory::default();
        assert!(inv.is_empty());
        inv.add(core_ids::stone(), 40);
        assert_eq!(inv.total_qty(), 40);
        assert_eq!(inv.take(core_ids::stone(), 25), 25);
        assert_eq!(inv.qty_of(core_ids::stone()), 15);
    }

    // ── Phase 6: chariot crew, draft & combat ─────────────────────────────

    #[test]
    fn vehicle_health_mirrors_design_durability() {
        let (_, registry) = load_vehicle_assets();
        let design = registry.by_name("Handcart").unwrap();
        let health = VehicleHealth::from_design(design);
        assert_eq!(health.cells.len(), design.grid.cells.len());
        assert!(health.cells.iter().all(|(_, hp)| *hp > 0));
        assert_eq!(health.intact_cells(), design.grid.cells.len());
        assert!(!health.movement_disabled());
    }

    #[test]
    fn crew_seat_count_matches_template() {
        let (_, registry) = load_vehicle_assets();
        assert_eq!(crew_seat_count(registry.by_name("Handcart").unwrap()), 0);
        assert_eq!(
            crew_seat_count(registry.by_name("Light Chariot").unwrap()),
            1
        );
    }

    #[test]
    fn destroying_a_wheel_disables_movement() {
        let (_, registry) = load_vehicle_assets();
        let design = registry.by_name("Handcart").unwrap().clone();
        let mut health = VehicleHealth::from_design(&design);
        // Handcart wheel at (0, 0, 0).
        let out = apply_vehicle_cell_damage(&mut health, &design, IVec3::new(0, 0, 0), 9_999);
        assert!(out.cell_destroyed && out.movement_disabled);
        assert!(health.movement_disabled());
    }

    #[test]
    fn destroying_cargo_bay_spills_and_hitch_releases() {
        let (_, registry) = load_vehicle_assets();
        let design = registry.by_name("Handcart").unwrap().clone();
        let mut health = VehicleHealth::from_design(&design);
        // CargoBay at (1, 2, 0), Hitch at (0, 3, 0).
        assert!(
            apply_vehicle_cell_damage(&mut health, &design, IVec3::new(1, 2, 0), 9_999)
                .spill_cargo
        );
        assert!(
            apply_vehicle_cell_damage(&mut health, &design, IVec3::new(0, 3, 0), 9_999)
                .release_animals
        );
    }

    #[test]
    fn destroying_low_frame_forces_overturn() {
        let (_, registry) = load_vehicle_assets();
        let design = registry.by_name("Handcart").unwrap().clone();
        let mut health = VehicleHealth::from_design(&design);
        // Frame at (0, 2, 0) — bottom Z, a structural support cell.
        let out = apply_vehicle_cell_damage(&mut health, &design, IVec3::new(0, 2, 0), 9_999);
        assert!(out.force_overturn, "a destroyed low support cell overturns");
    }

    #[test]
    fn partial_damage_leaves_cell_standing() {
        let (_, registry) = load_vehicle_assets();
        let design = registry.by_name("Handcart").unwrap().clone();
        let mut health = VehicleHealth::from_design(&design);
        let out = apply_vehicle_cell_damage(&mut health, &design, IVec3::new(0, 0, 0), 1);
        assert!(!out.cell_destroyed);
        assert!(health.cell_health(IVec3::new(0, 0, 0)) > 0);
    }

    #[test]
    fn hit_location_biases_low_cells() {
        // Two stacked cells; the low one should be hit far more often.
        let health = VehicleHealth {
            cells: vec![(IVec3::new(0, 0, 0), 100), (IVec3::new(0, 0, 1), 100)],
            disabled: VehicleDisableFlags::default(),
        };
        let mut low = 0;
        for _ in 0..600 {
            if pick_hit_cell(&health).unwrap().z == 0 {
                low += 1;
            }
        }
        assert!(low > 300, "melee should bias the low cell ({low}/600)");
    }
}
