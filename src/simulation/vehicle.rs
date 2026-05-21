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
use crate::simulation::faction::StorageTileMap;
use crate::simulation::items::GroundItem;
use crate::simulation::schedule::SimClock;
use crate::simulation::technology::{
    TechId, ANIMAL_HUSBANDRY, BRONZE_CASTING, HORSE_TAMING, OX_CART, WAR_CHARIOT,
};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::tile_to_world;

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
            },
            VehicleInventory::default(),
            VehicleCrew::default(),
            VehicleDraft {
                hitched: Vec::new(),
                required_animals: design.required_animals,
            },
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
    fn inventory_add_take_roundtrip() {
        let mut inv = VehicleInventory::default();
        assert!(inv.is_empty());
        inv.add(core_ids::stone(), 40);
        assert_eq!(inv.total_qty(), 40);
        assert_eq!(inv.take(core_ids::stone(), 25), 25);
        assert_eq!(inv.qty_of(core_ids::stone()), 15);
    }
}
