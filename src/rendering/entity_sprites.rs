use crate::economy::resource_catalog::ResourceCatalog;
use crate::rendering::pixel_art::{AnimalTextures, ArtMode, EntityTextures};
use crate::rendering::sprite_library::SpriteLibrary;
use crate::simulation::animals::{Cat, Cow, Deer, Fox, Horse, Pig, Rabbit, Wolf};
use crate::simulation::vehicle::{
    Vehicle, VehicleData, VehicleDesign, VehicleDesignRegistry, VehiclePartKind, VehicleVisual,
};
use crate::simulation::construction::{
    Bed, Blueprint, BuildSiteKind, Campfire, Chair, Door, Loom, ShelterTier, Table, TentShelter,
    Wall, WallMaterial, Well, Workbench,
};
use crate::simulation::faction::{
    FactionCenter, FactionMember, PlayerFaction, PlayerFactionMarker,
};
use crate::simulation::items::{Equipment, EquipmentSlot, GroundItem};
use crate::simulation::person::{HairColor, Person, PersonAI, SkinTone};
use crate::rendering::plant_sprites::PlantSpriteSet;
use crate::simulation::plant_catalog::{PlantForm, PlantSpeciesId};
use crate::simulation::plants::{GrowthStage, Plant, PlantKind, PlantSpecies, PlantSpriteVariant};
use crate::simulation::reproduction::BiologicalSex;
use crate::world::terrain::tile_to_world;
use bevy::prelude::*;

use bevy::sprite::Anchor;

/// Note: All entity sprites in this game follow a unified alignment rule:
/// 1. Logical entities are spawned at the mathematical center of their tile (wx, wy).
/// 2. Sprites are attached as children with `Anchor::BottomCenter`.
/// 3. To align the sprite's bottom edge with the tile's visual bottom,
///    a universal Y-offset of -8.0 is applied to the child transform.
/// This ensures that 16px tall walls and 36px tall people both stand on the same floor.

#[derive(Component, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntityFogState {
    #[default]
    Visible,
    Explored,
}

/// Marker for entities that should remain visible (dimmed) in explored-but-not-
/// currently-visible tiles. Attach to static structures whose position the player
/// is expected to remember (walls, beds, plants, blueprints, faction centers,
/// furniture). Mobile entities (persons, animals) deliberately omit this marker
/// so they hide entirely outside line of sight — their remembered positions
/// would be misleading since they move.
#[derive(Component)]
pub struct FogPersistent;

#[derive(Component)]
pub struct BedVisual;

#[derive(Component)]
pub struct WallVisual;

#[derive(Component)]
pub struct WolfVisual;

#[derive(Component)]
pub struct DeerVisual;

#[derive(Component)]
pub struct HorseVisual;

#[derive(Component)]
pub struct CowVisual;

#[derive(Component)]
pub struct RabbitVisual;

#[derive(Component)]
pub struct PigVisual;

#[derive(Component)]
pub struct FoxVisual;

#[derive(Component)]
pub struct CatVisual;

#[derive(Component)]
pub struct PersonVisual;

/// Floating name label Text2d child. Does NOT carry VisualChild — fog-tint and art-mode systems skip it.
#[derive(Component)]
pub struct PersonNameLabel;

#[derive(Component)]
pub struct FactionCenterVisual;

#[derive(Component)]
pub struct PlantVisual;

#[derive(Component)]
pub struct BlueprintVisual;

#[derive(Component)]
pub struct CampfireVisual;

#[derive(Component)]
pub struct DoorVisual;

#[derive(Component)]
pub struct TableVisual;

#[derive(Component)]
pub struct ChairVisual;

#[derive(Component)]
pub struct WorkbenchVisual;

#[derive(Component)]
pub struct LoomVisual;

#[derive(Component)]
pub struct WellVisual;

#[derive(Component)]
pub struct TentShelterVisual;

#[derive(Component)]
pub struct GroundItemVisual;

/// Base tint color on a VisualChild entity — used by the fog system to preserve
/// sex-based coloring while still applying fog darkening.
#[derive(Component, Clone, Copy)]
pub struct AnimalSexTint(pub Color);

/// Base tint color on a vehicle cell `VisualChild` — `apply_entity_fog_tint_system`
/// multiplies it by the fog factor so the part-kind palette
/// (`vehicle_cell_color`) survives instead of being clobbered to white.
/// Mirrors `AnimalSexTint`; per-cell because every cell carries its own
/// `vehicle_cell_color(kind)` colour.
#[derive(Component, Clone, Copy)]
pub struct VehicleCellTint(pub Color);


#[derive(Component)]
pub struct VisualChild;

/// Identifies which rendering layer a VisualChild belongs to.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum VisualLayer {
    Body,
    Clothing,
    Hair,
}

/// Cached clothing color key derived from equipped TorsoArmor material.
#[derive(Component, Clone)]
pub struct ClothingVisual {
    pub color_key: &'static str,
    pub visible: bool,
}

impl Default for ClothingVisual {
    fn default() -> Self {
        Self {
            color_key: "tan",
            visible: false,
        }
    }
}

/// Tracks the previous-frame world position for direction inference on non-person entities.
#[derive(Component, Default)]
pub struct LastPos(pub Vec2);

#[derive(Component, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FacingDirection {
    #[default]
    South = 0,
    SouthEast = 1,
    East = 2,
    NorthEast = 3,
    North = 4,
    NorthWest = 5,
    West = 6,
    SouthWest = 7,
}

impl FacingDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::South => "s",
            Self::SouthEast => "se",
            Self::East => "e",
            Self::NorthEast => "ne",
            Self::North => "n",
            Self::NorthWest => "nw",
            Self::West => "w",
            Self::SouthWest => "sw",
        }
    }

    /// Cardinal-only suffix used by the legacy procedural sprite library
    /// (`anim_<species>_{s,n,e,w}_<frame>`). Diagonals collapse to their
    /// dominant cardinal so existing 4-way ASCII art keeps working.
    pub fn cardinal_str(self) -> &'static str {
        match self {
            Self::South | Self::SouthEast | Self::SouthWest => "s",
            Self::North | Self::NorthEast | Self::NorthWest => "n",
            Self::East => "e",
            Self::West => "w",
        }
    }

    /// 8-sector bucketing of a movement vector. Returns `current` when the
    /// movement magnitude is below the noise threshold so idle animals keep
    /// their last-known facing.
    pub fn from_velocity(diff: Vec2, current: FacingDirection) -> FacingDirection {
        if diff.length_squared() < 0.25 {
            return current;
        }
        let angle = diff.y.atan2(diff.x);
        let sector = ((angle / std::f32::consts::FRAC_PI_4).round() as i32).rem_euclid(8);
        match sector {
            0 => Self::East,
            1 => Self::NorthEast,
            2 => Self::North,
            3 => Self::NorthWest,
            4 => Self::West,
            5 => Self::SouthWest,
            6 => Self::South,
            _ => Self::SouthEast,
        }
    }
}

pub fn spawn_bed_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Bed), Without<BedVisual>>,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
) {
    for (entity, bed) in query.iter() {
        // A poor-housing sleeping mat finalises as a Bed at the SleepingMat
        // tier; render it as a flat woven mat, not a bed frame.
        let img = if bed.tier == crate::simulation::construction::BedTier::SleepingMat {
            sprite_lib
                .get("building_sleeping_mat")
                .cloned()
                .unwrap_or_else(|| textures.bed_ascii.clone())
        } else {
            textures.bed_ascii.clone()
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands
            .entity(entity)
            .insert((BedVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_wall_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Wall), Without<WallVisual>>,
    textures: Res<EntityTextures>,
) {
    for (entity, wall) in query.iter() {
        let img = match wall.material {
            WallMaterial::Palisade => textures.wall_palisade_ascii.clone(),
            WallMaterial::WattleDaub => textures.wall_wattle_ascii.clone(),
            WallMaterial::Stone => textures.wall_stone_ascii.clone(),
            WallMaterial::Mudbrick => textures.wall_mudbrick_ascii.clone(),
            WallMaterial::CutStone => textures.wall_cutstone_ascii.clone(),
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands
            .entity(entity)
            .insert((WallVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_campfire_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Campfire>, Without<CampfireVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let img = textures.campfire_ascii.clone();

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands
            .entity(entity)
            .insert((CampfireVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_door_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Door>, Without<DoorVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.door_ascii.clone());
        sprite.anchor = Anchor::BottomCenter;

        commands
            .entity(entity)
            .insert((DoorVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_table_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Table>, Without<TableVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.table_ascii.clone());
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((TableVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_chair_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Chair>, Without<ChairVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.chair_ascii.clone());
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((ChairVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

/// Flat tint for one vehicle cell, keyed by part kind — the freeform
/// composed-sprite renderer's palette.
pub fn vehicle_cell_color(kind: VehiclePartKind) -> Color {
    match kind {
        VehiclePartKind::Frame => Color::srgb(0.45, 0.30, 0.16),
        VehiclePartKind::Deck => Color::srgb(0.62, 0.46, 0.28),
        VehiclePartKind::Wall => Color::srgb(0.55, 0.55, 0.58),
        VehiclePartKind::Axle => Color::srgb(0.28, 0.24, 0.22),
        VehiclePartKind::Wheel => Color::srgb(0.12, 0.10, 0.10),
        VehiclePartKind::Hitch | VehiclePartKind::Yoke => Color::srgb(0.34, 0.22, 0.12),
        VehiclePartKind::CargoBay => Color::srgb(0.74, 0.60, 0.36),
        VehiclePartKind::CrewSeat => Color::srgb(0.72, 0.28, 0.22),
        VehiclePartKind::WeaponMount => Color::srgb(0.50, 0.16, 0.14),
        VehiclePartKind::Engine
        | VehiclePartKind::Track
        | VehiclePartKind::ArmorPlate
        | VehiclePartKind::Turret => Color::srgb(0.40, 0.42, 0.46),
    }
}

/// One painted quad in a composed vehicle render.
#[derive(Clone, Debug)]
pub struct VehicleSpriteCell {
    /// Entity-local offset (x, y, z-order).
    pub local_offset: Vec3,
    /// Quad size in pixels.
    pub size: Vec2,
    /// Base tint — the fallback when no sprite is registered, and the tint
    /// `apply_entity_fog_tint_system` multiplies against the loaded sprite.
    pub color: Color,
    /// Optional `SpriteLibrary` key. `spawn_vehicle_sprites` tries this
    /// first, then [`fallback_sprite_key`], then paints a colour quad.
    /// `vehicle_sprite_plan` returns `None` here; the data-aware
    /// `vehicle_sprite_plan_with_data` populates it.
    pub sprite_key: Option<String>,
    /// Base-key fallback (e.g. `vehicle_wheel_base_side` for a
    /// `vehicle_wheel_large_offroad_wheel_side` cell whose variant has no
    /// distinct sprite registered). Skipped when `None`.
    pub fallback_sprite_key: Option<String>,
    /// Horizontal flip applied to the sprite when rendered (heading mirror —
    /// west uses the east-facing side sprite flipped, see
    /// `vehicle_part_sprites::view_for_heading`).
    pub flip_x: bool,
}

/// The render plan for one vehicle design at a given heading. Both
/// `spawn_vehicle_sprites` (world entity) and the egui inset preview in the
/// vehicle designer consume this, so cell composition can never drift.
#[derive(Clone, Debug)]
pub enum VehicleSpritePlan {
    /// Hand-drawn `entity_cart` sprite (stock flat designs).
    Stock,
    /// Composed body — one colored quad per cell, already heading-rotated.
    Composed { cells: Vec<VehicleSpriteCell> },
}

/// Rotate an entity-local (x, y) offset by `heading` quarter-turns CCW.
/// Heading 0 = forward +Y; matches `VehicleFootprint::offsets_by_heading`.
fn rotate_xy(x: f32, y: f32, heading: u8) -> (f32, f32) {
    match heading % 4 {
        0 => (x, y),
        1 => (-y, x),
        2 => (-x, -y),
        _ => (y, -x),
    }
}

/// Pure helper: build the sprite plan for `design` facing `heading`. No Bevy
/// resources required. The composed branch reproduces the layout
/// `spawn_vehicle_sprites` used pre-extraction, with explicit heading rotation
/// applied to the in-plane (x, y) cell offsets (the legacy renderer assumed
/// heading 0 because the parent `Transform` had no rotation).
///
/// Backward-compatible wrapper that returns a colour-only plan (no sprite
/// keys, no module composites — used by the designer's egui inset preview
/// and headless tests). The world renderer calls
/// [`vehicle_sprite_plan_with_data`] to get the sprite-aware plan.
pub fn vehicle_sprite_plan(design: &VehicleDesign, heading: u8) -> VehicleSpritePlan {
    vehicle_sprite_plan_internal(design, heading, None)
}

/// Same as [`vehicle_sprite_plan`] but with [`VehicleData`] in hand so each
/// cell carries the right sprite key (kind + variant + view) and weapon
/// modules collapse to one composite sprite at their anchor cell.
pub fn vehicle_sprite_plan_with_data(
    design: &VehicleDesign,
    heading: u8,
    data: &VehicleData,
) -> VehicleSpritePlan {
    vehicle_sprite_plan_internal(design, heading, Some(data))
}

fn vehicle_sprite_plan_internal(
    design: &VehicleDesign,
    heading: u8,
    data: Option<&VehicleData>,
) -> VehicleSpritePlan {
    use crate::rendering::vehicle_part_sprites::{
        module_anchor_cell, module_footprint_extent, vehicle_module_sprite_key,
        vehicle_part_sprite_key, view_for_heading,
    };

    let bounds = design.grid.bounds();
    // Only fall through to the hand-drawn Stock cart sprite when there is
    // genuinely no per-cell data to compose. The data-aware path always has
    // sprite keys available (`VehicleData` in hand), so spawned vehicles
    // render the composed grid + asymmetric per-view art instead of the
    // single static cart fallback. The colour-only `vehicle_sprite_plan`
    // wrapper (designer preview / headless tests) still routes here with
    // `data == None`, where Stock keeps producing a sensible placeholder.
    let stock_flat = data.is_none()
        && design.author_faction.is_none()
        && bounds.map(|(lo, hi)| hi.z - lo.z == 0).unwrap_or(true);
    if stock_flat {
        return VehicleSpritePlan::Stock;
    }
    let Some((lo, hi)) = bounds else {
        return VehicleSpritePlan::Composed { cells: Vec::new() };
    };
    let w = (hi.x - lo.x + 1) as f32;
    let depth = (hi.y - lo.y + 1) as f32;
    // One grid cell == one world tile (`TILE_SIZE = 16 px`) in both X and Y
    // so adjacent cells render edge-to-edge, with no sub-tile gap. The
    // Tilted-view projection layer (`ProjectedAnchor::Dynamic` auto-attached
    // for `Vehicle`) handles Y-axis foreshortening — doing it here too
    // would double-compress the body in Tilted mode and *under*-compress
    // it in TopDown.
    const CELL_PX: f32 = 16.0;
    let (view, flip_x) = view_for_heading(heading);

    // Pre-compute each weapon module's anchor cell so we can skip the
    // non-anchor members and emit one composite at the anchor.
    let module_anchors: ahash::AHashMap<crate::simulation::vehicle::VehicleModuleId, bevy::math::IVec3> =
        design
            .grid
            .modules
            .iter()
            .filter_map(|inst| module_anchor_cell(&inst.cells).map(|a| (inst.id, a)))
            .collect();

    // Project a (gx, gy, gz) cell-grid coord into entity-local screen offset.
    let project = |gx: f32, gy: f32, gz: f32| -> Vec3 {
        let cx = (gx - (w - 1.0) / 2.0) * CELL_PX;
        let cy_in_plane = (gy - (depth - 1.0) / 2.0) * CELL_PX;
        let (rx, ry) = rotate_xy(cx, cy_in_plane, heading);
        let sx = rx;
        let sy = -8.0 + gz * CELL_PX + ry;
        let zorder = 0.1 + gz * 0.002 + gy * 0.0003;
        Vec3::new(sx, sy, zorder)
    };

    let mut cells = Vec::with_capacity(design.grid.cells.len());
    for (p, c) in &design.grid.cells {
        // Module-composite anchor cell: emit ONE sprite spanning the
        // module's footprint, sized to (extent_x × extent_y) cells.
        if let Some(mid) = c.module_id {
            if module_anchors.get(&mid) != Some(p) {
                // Non-anchor cell of a multi-cell module — the anchor
                // already drew the composite for this module.
                continue;
            }
            if let Some(data) = data {
                if let Some(inst) =
                    design.grid.modules.iter().find(|m| m.id == mid)
                {
                    let (ext_x, ext_y) = module_footprint_extent(&inst.cells);
                    if let Some(def) = data.module_def(inst.def) {
                        // Position the composite at the footprint centre
                        // so a 2×2 module sits over its 4 grid cells.
                        let center_x =
                            (inst.cells.iter().map(|c| c.x).sum::<i32>() as f32)
                                / (inst.cells.len() as f32)
                                - lo.x as f32;
                        let center_y =
                            (inst.cells.iter().map(|c| c.y).sum::<i32>() as f32)
                                / (inst.cells.len() as f32)
                                - lo.y as f32;
                        let center_z = (p.z - lo.z) as f32;
                        let local = project(center_x, center_y, center_z);
                        let size = Vec2::new(
                            ext_x as f32 * CELL_PX,
                            ext_y as f32 * CELL_PX,
                        );
                        cells.push(VehicleSpriteCell {
                            local_offset: local,
                            size,
                            color: vehicle_cell_color(c.kind),
                            sprite_key: Some(vehicle_module_sprite_key(
                                &def.label, view,
                            )),
                            // Module composites have no per-kind base
                            // fallback; an unregistered module silently
                            // drops to the colour quad.
                            fallback_sprite_key: None,
                            flip_x,
                        });
                        continue;
                    }
                }
            }
            // No data available (designer preview / fallback): treat the
            // module cells like ordinary tinted cells.
        }

        let gx = (p.x - lo.x) as f32;
        let gy = (p.y - lo.y) as f32;
        let gz = (p.z - lo.z) as f32;
        let local = project(gx, gy, gz);
        let (sprite_key, fallback_sprite_key) = if let Some(d) = data {
            let variant_label = c
                .variant
                .and_then(|vid| d.variant(vid))
                .map(|v| v.label.as_str());
            let primary = vehicle_part_sprite_key(c.kind, variant_label, view);
            let fallback = if variant_label.is_some() {
                Some(vehicle_part_sprite_key(c.kind, None, view))
            } else {
                None
            };
            (Some(primary), fallback)
        } else {
            (None, None)
        };
        cells.push(VehicleSpriteCell {
            local_offset: local,
            size: Vec2::splat(CELL_PX),
            color: vehicle_cell_color(c.kind),
            sprite_key,
            fallback_sprite_key,
            flip_x,
        });
    }

    // ── Connector overlay pass ───────────────────────────────────────
    // Adjacency-aware bridging hardware: axle↔wheel hubs, same-kind frame/
    // deck/wall seams, hitch/yoke attachments, and per-cell crew-seat
    // facing indicators. Overlays are 16×16, painted at +0.001 z so they
    // sit just above the base cell, and use `flip_x = false` so the
    // computed screen direction matches the registered ASCII regardless
    // of the base-cell mirror (the screen-direction is already baked into
    // the picked sprite key via `grid_delta_to_screen_dir`).
    push_connector_overlays(
        design, lo, heading, view, project, &module_anchors, &mut cells,
    );

    VehicleSpritePlan::Composed { cells }
}

/// Map a 6-direction grid delta into the camera-space `ConnectorDir`
/// (Up / Down / Left / Right) for the given heading. Returns `None` only
/// for the zero vector; the six `NEIGHBORS_6` deltas always resolve.
fn grid_delta_to_screen_dir(
    delta: bevy::math::IVec3,
    heading: u8,
) -> Option<crate::rendering::vehicle_part_sprites::ConnectorDir> {
    use crate::rendering::vehicle_part_sprites::ConnectorDir;
    if delta.z != 0 {
        return Some(if delta.z > 0 {
            ConnectorDir::Up
        } else {
            ConnectorDir::Down
        });
    }
    // In-plane (x, y) grid delta rotates with heading the same way
    // `rotate_xy` rotates cell positions, so a +Y neighbour in the grid
    // lands on the screen-edge that `rotate_xy(0, 1, heading)` projects to.
    let (sx, sy) = rotate_xy(delta.x as f32, delta.y as f32, heading);
    if sx.abs() > sy.abs() {
        Some(if sx > 0.0 {
            ConnectorDir::Right
        } else {
            ConnectorDir::Left
        })
    } else if sy.abs() > 0.0 {
        Some(if sy > 0.0 {
            ConnectorDir::Up
        } else {
            ConnectorDir::Down
        })
    } else {
        None
    }
}

fn push_connector_overlays<F: Fn(f32, f32, f32) -> Vec3>(
    design: &VehicleDesign,
    lo: bevy::math::IVec3,
    heading: u8,
    view: crate::rendering::vehicle_part_sprites::VehicleSpriteView,
    project: F,
    module_anchors: &ahash::AHashMap<
        crate::simulation::vehicle::VehicleModuleId,
        bevy::math::IVec3,
    >,
    cells: &mut Vec<VehicleSpriteCell>,
) {
    use crate::rendering::vehicle_part_sprites::vehicle_connector_sprite_key;
    use bevy::math::IVec3;

    // Six axis-aligned grid neighbours, same set `vehicle::validate_grid`
    // walks for wheel↔axle connectivity validation.
    const NEIGHBORS_6: [IVec3; 6] = [
        IVec3::new(1, 0, 0),
        IVec3::new(-1, 0, 0),
        IVec3::new(0, 1, 0),
        IVec3::new(0, -1, 0),
        IVec3::new(0, 0, 1),
        IVec3::new(0, 0, -1),
    ];

    let kind_at: ahash::AHashMap<IVec3, VehiclePartKind> = design
        .grid
        .cells
        .iter()
        .filter(|(_, c)| c.module_id.is_none())
        .map(|(p, c)| (*p, c.kind))
        .collect();

    let chassis_forward_screen = grid_delta_to_screen_dir(IVec3::new(0, 1, 0), heading);

    for (p, c) in &design.grid.cells {
        // Skip module cells entirely — their composite sprite already
        // covers the footprint; per-cell overlays would clash.
        if let Some(mid) = c.module_id {
            if module_anchors.get(&mid) != Some(p) {
                continue;
            }
            // Anchor cells of multi-cell modules also skip overlay
            // emission — the module composite owns its silhouette.
            continue;
        }

        let gx = (p.x - lo.x) as f32;
        let gy = (p.y - lo.y) as f32;
        let gz = (p.z - lo.z) as f32;
        let local = project(gx, gy, gz);
        let mut overlay_z = local.z + 0.001;

        // Per-cell seat-facing indicator — no neighbour required; the
        // indicator points at chassis-forward in screen space.
        if c.kind == VehiclePartKind::CrewSeat {
            if let Some(dir) = chassis_forward_screen {
                cells.push(VehicleSpriteCell {
                    local_offset: Vec3::new(local.x, local.y, overlay_z),
                    size: Vec2::splat(16.0),
                    color: vehicle_cell_color(c.kind),
                    sprite_key: Some(vehicle_connector_sprite_key(
                        "crew_seat_facing",
                        view,
                        dir,
                    )),
                    fallback_sprite_key: None,
                    flip_x: false,
                });
                overlay_z += 0.0005;
            }
        }

        // Adjacency-driven overlays.
        for delta in NEIGHBORS_6 {
            let Some(&n_kind) = kind_at.get(&(*p + delta)) else {
                continue;
            };
            let label: Option<&'static str> = match (c.kind, n_kind) {
                (VehiclePartKind::Axle, VehiclePartKind::Wheel) => Some("axle_wheel"),
                (VehiclePartKind::Frame, VehiclePartKind::Frame) => Some("frame_seam"),
                (VehiclePartKind::Deck, VehiclePartKind::Deck) => Some("deck_seam"),
                (VehiclePartKind::Wall, VehiclePartKind::Wall) => Some("wall_seam"),
                (VehiclePartKind::Hitch, VehiclePartKind::Frame) => Some("hitch_attach"),
                (VehiclePartKind::Yoke, VehiclePartKind::Frame) => Some("yoke_attach"),
                _ => None,
            };
            let Some(label) = label else {
                continue;
            };
            let Some(dir) = grid_delta_to_screen_dir(delta, heading) else {
                continue;
            };
            cells.push(VehicleSpriteCell {
                local_offset: Vec3::new(local.x, local.y, overlay_z),
                size: Vec2::splat(16.0),
                color: vehicle_cell_color(c.kind),
                sprite_key: Some(vehicle_connector_sprite_key(label, view, dir)),
                fallback_sprite_key: None,
                flip_x: false,
            });
            overlay_z += 0.0005;
        }
    }
}

/// Spawn `VisualChild` cells for one vehicle entity. Used by
/// `refresh_vehicle_sprites_system` for both first-time attach and
/// post-rebuild repopulation. Builds the data-aware sprite plan and
/// spawns one colour-quad-or-sprite child per cell. Stock 1-tall designs
/// (designer preview path with no `VehicleData`) reuse the hand-drawn
/// `entity_cart`; the in-world path always composes from cells.
fn spawn_vehicle_visual_cells(
    commands: &mut Commands,
    parent: Entity,
    design: &VehicleDesign,
    heading: u8,
    sprite_lib: &SpriteLibrary,
    data: &VehicleData,
) {
    match vehicle_sprite_plan_with_data(design, heading, data) {
        VehicleSpritePlan::Stock => {
            let Some(handle) = sprite_lib.get("entity_cart") else {
                return;
            };
            let mut sprite = Sprite::from_image(handle.clone());
            sprite.anchor = Anchor::BottomCenter;
            commands.entity(parent).with_children(|p| {
                p.spawn((
                    VisualChild,
                    sprite,
                    Transform::from_xyz(0.0, -8.0, 0.1),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        }
        VehicleSpritePlan::Composed { cells } => {
            commands.entity(parent).with_children(|p| {
                for cell in cells {
                    let resolved = cell
                        .sprite_key
                        .as_deref()
                        .and_then(|key| sprite_lib.get(key).cloned())
                        .or_else(|| {
                            cell.fallback_sprite_key
                                .as_deref()
                                .and_then(|key| sprite_lib.get(key).cloned())
                        });
                    let mut sprite = match resolved {
                        Some(img) => {
                            let mut s = Sprite::from_image(img);
                            s.flip_x = cell.flip_x;
                            s.custom_size = Some(cell.size);
                            s
                        }
                        None => Sprite::from_color(cell.color, cell.size),
                    };
                    sprite.anchor = Anchor::BottomCenter;
                    p.spawn((
                        VisualChild,
                        // `VehicleCellTint` survives the fog-tint sweep
                        // (`apply_entity_fog_tint_system`) AND exempts the
                        // cell from `update_animations`' (0,-8) rest-position
                        // reset, which assumes a single per-entity child.
                        VehicleCellTint(cell.color),
                        sprite,
                        Transform::from_xyz(
                            cell.local_offset.x,
                            cell.local_offset.y,
                            cell.local_offset.z,
                        ),
                        GlobalTransform::default(),
                        Visibility::Inherited,
                        InheritedVisibility::default(),
                    ));
                }
            });
        }
    }
}

/// Vehicle render driver — attaches `VisualChild` cells on first sight and
/// rebuilds them whenever `Vehicle.heading % 4` or `Vehicle.state` changes
/// (the two axes that flip per-cell sprite art via `view_for_heading` /
/// asymmetric `_BACK` variants). Other `Vehicle` field churn (anchor_tile,
/// z, hauler) does **not** trigger a refresh — the parent `Transform`
/// already moves the children along for free.
///
/// Vehicles are mobile, so — like Person / animal sprites — they omit
/// `FogPersistent`. Only `VisualChild`-marked children are torn down on
/// rebuild; `EntityFogState`, hauler joints, etc. survive.
pub fn refresh_vehicle_sprites_system(
    mut commands: Commands,
    new_q: Query<(Entity, &Vehicle), Without<VehicleVisual>>,
    mut refresh_q: Query<(Entity, &Vehicle, &mut VehicleVisual, Option<&Children>)>,
    visual_child_q: Query<(), With<VisualChild>>,
    registry: Res<VehicleDesignRegistry>,
    sprite_lib: Res<SpriteLibrary>,
    data: Res<VehicleData>,
) {
    // First-attach pass.
    for (entity, vehicle) in new_q.iter() {
        let bucket = vehicle.heading % 4;
        commands.entity(entity).insert((
            VehicleVisual {
                design_id: vehicle.design_id,
                heading_bucket: bucket,
                state: vehicle.state,
            },
            EntityFogState::default(),
        ));
        let Some(design) = registry.get(vehicle.design_id) else {
            continue;
        };
        spawn_vehicle_visual_cells(
            &mut commands,
            entity,
            design,
            vehicle.heading,
            &sprite_lib,
            &data,
        );
    }

    // Refresh pass: rebuild children when heading bucket / state flips.
    for (entity, vehicle, mut visual, children) in refresh_q.iter_mut() {
        let bucket = vehicle.heading % 4;
        if visual.heading_bucket == bucket
            && visual.state == vehicle.state
            && visual.design_id == vehicle.design_id
        {
            continue;
        }
        visual.heading_bucket = bucket;
        visual.state = vehicle.state;
        visual.design_id = vehicle.design_id;

        // Despawn only `VisualChild`-marked children — leaves hauler joints,
        // crew indicators, name labels, etc. intact.
        if let Some(children) = children {
            for &child in children.iter() {
                if visual_child_q.get(child).is_ok() {
                    commands.entity(child).despawn();
                }
            }
        }

        let Some(design) = registry.get(vehicle.design_id) else {
            continue;
        };
        spawn_vehicle_visual_cells(
            &mut commands,
            entity,
            design,
            vehicle.heading,
            &sprite_lib,
            &data,
        );
    }
}


pub fn spawn_workbench_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Workbench>, Without<WorkbenchVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.workbench_ascii.clone());
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((WorkbenchVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_loom_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Loom>, Without<LoomVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.loom_ascii.clone());
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((LoomVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_well_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Well>, Without<WellVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
) {
    let Some(handle) = sprite_lib.get("building_well") else {
        return;
    };
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(handle.clone());
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((WellVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

/// Render finished lightweight shelters (lean-to / tent / yurt). Each
/// `ShelterTier` maps to its own procedural pixel sprite; falls back to the
/// palisade-wall sprite only if the art isn't registered.
pub fn spawn_tent_shelter_sprites(
    mut commands: Commands,
    query: Query<(Entity, &TentShelter), Without<TentShelterVisual>>,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
) {
    for (entity, shelter) in query.iter() {
        let key = match shelter.tier {
            ShelterTier::LeanTo => "building_lean_to",
            ShelterTier::Tent => "building_tent",
            ShelterTier::Yurt => "building_yurt",
        };
        let img = sprite_lib
            .get(key)
            .cloned()
            .unwrap_or_else(|| textures.wall_palisade_ascii.clone());
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        commands
            .entity(entity)
            .insert((TentShelterVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_ground_item_sprites(
    mut commands: Commands,
    query: Query<(Entity, &GroundItem), Without<GroundItemVisual>>,
    sprite_lib: Res<SpriteLibrary>,
    catalog: Res<ResourceCatalog>,
) {
    for (entity, gi) in query.iter() {
        let id = gi.item.resource_id;
        let Some(def) = catalog.get(id) else { continue };
        let Some(key) = def.sprite_key.as_deref() else {
            continue;
        };
        let Some(img) = sprite_lib.get(key).cloned() else {
            continue;
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert((
            GroundItemVisual,
            EntityFogState::default(),
            FogPersistent,
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.5),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_faction_center_sprites(
    mut commands: Commands,
    query: Query<
        (Entity, Option<&PlayerFactionMarker>),
        (With<FactionCenter>, Without<FactionCenterVisual>),
    >,
    textures: Res<EntityTextures>,
) {
    for (entity, player_marker) in query.iter() {
        let img = textures.camp_ascii.clone();

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        if player_marker.is_some() {
            sprite.color = Color::srgb(0.55, 0.85, 1.0);
        }

        commands.entity(entity).insert((
            FactionCenterVisual,
            EntityFogState::default(),
            FogPersistent,
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_wolf_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&crate::simulation::reproduction::BiologicalSex>,
        ),
        (With<Wolf>, Without<WolfVisual>),
    >,
    textures: Res<EntityTextures>,
    animal_tex: Res<AnimalTextures>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Ascii {
            textures.wolf_ascii.clone()
        } else {
            animal_tex.wolf[FacingDirection::South as usize].clone()
        };

        // Male wolves are slightly grey; females are reference white
        let tint = match sex_opt {
            Some(crate::simulation::reproduction::BiologicalSex::Female) => Color::WHITE,
            _ => Color::srgb(0.75, 0.75, 0.75),
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;

        commands.entity(entity).insert((
            WolfVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_deer_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&crate::simulation::reproduction::BiologicalSex>,
        ),
        (With<Deer>, Without<DeerVisual>),
    >,
    textures: Res<EntityTextures>,
    animal_tex: Res<AnimalTextures>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Pixel {
            animal_tex.deer[FacingDirection::South as usize].clone()
        } else {
            textures.deer_ascii.clone()
        };

        // Male deer are warm tan; females are lighter cream
        let tint = match sex_opt {
            Some(crate::simulation::reproduction::BiologicalSex::Male) | None => {
                Color::srgb(0.80, 0.65, 0.48)
            }
            Some(crate::simulation::reproduction::BiologicalSex::Female) => {
                Color::srgb(0.95, 0.88, 0.78)
            }
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;

        commands.entity(entity).insert((
            DeerVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_person_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&BiologicalSex>,
            Option<&SkinTone>,
            Option<&HairColor>,
            Option<&ClothingVisual>,
            Option<&Name>,
            Option<&FactionMember>,
        ),
        (With<Person>, Without<PersonVisual>),
    >,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
    player_faction: Res<PlayerFaction>,
) {
    for (entity, sex_opt, tone_opt, hair_opt, clothing_opt, name_opt, faction_opt) in query.iter() {
        let is_female = matches!(sex_opt, Some(BiologicalSex::Female));
        let sex_str = if is_female { "female" } else { "male" };

        let mut entity_cmds = commands.entity(entity);
        entity_cmds.insert((
            PersonVisual,
            FacingDirection::South,
            EntityFogState::default(),
        ));
        if clothing_opt.is_none() {
            entity_cmds.insert(ClothingVisual::default());
        }

        if *art_mode == ArtMode::Ascii {
            let img = if is_female {
                textures.person_female_ascii.clone()
            } else {
                textures.person_male_ascii.clone()
            };
            let mut sprite = Sprite::from_image(img);
            sprite.color = Color::WHITE;
            sprite.anchor = Anchor::BottomCenter;
            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    VisualChild,
                    VisualLayer::Body,
                    sprite,
                    Transform::from_xyz(0.0, -8.0, 0.0),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        } else {
            let tone_str = tone_opt.map(|t| t.as_str()).unwrap_or("tan");
            let hair_str = hair_opt.map(|h| h.as_str()).unwrap_or("brown");
            let cloth_str = clothing_opt.map(|c| c.color_key).unwrap_or("tan");

            let body_img = sprite_lib
                .get(&format!("body_{sex_str}_{tone_str}_s_a"))
                .cloned()
                .unwrap_or_else(|| textures.person_male_ascii.clone());
            let hair_img = sprite_lib
                .get(&format!("hair_{sex_str}_{hair_str}_s_a"))
                .cloned()
                .unwrap_or_else(|| textures.person_male_ascii.clone());
            let cloth_img = sprite_lib
                .get(&format!("clothing_{sex_str}_{cloth_str}_s_a"))
                .cloned()
                .unwrap_or_else(|| textures.person_male_ascii.clone());

            let mut body_sprite = Sprite::from_image(body_img);
            body_sprite.color = Color::WHITE;
            body_sprite.anchor = Anchor::BottomCenter;

            let mut hair_sprite = Sprite::from_image(hair_img);
            hair_sprite.anchor = Anchor::BottomCenter;

            let mut cloth_sprite = Sprite::from_image(cloth_img);
            cloth_sprite.color = Color::NONE;
            cloth_sprite.anchor = Anchor::BottomCenter;

            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    VisualChild,
                    VisualLayer::Body,
                    body_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.0),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
                parent.spawn((
                    VisualChild,
                    VisualLayer::Clothing,
                    cloth_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.1),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
                parent.spawn((
                    VisualChild,
                    VisualLayer::Hair,
                    hair_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.2),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        }

        let is_player = faction_opt.map_or(false, |m| m.faction_id == player_faction.faction_id);
        if is_player {
            let label_text = name_opt.map(|n| n.as_str().to_string()).unwrap_or_default();
            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    PersonNameLabel,
                    Text2d::new(label_text),
                    TextFont {
                        font_size: 8.0,
                        ..default()
                    },
                    TextColor(Color::WHITE),
                    TextLayout::default(),
                    // Sprite is 16px tall, bottom at Y=-8 → top at Y=+8; +3px gap
                    Transform::from_xyz(0.0, 11.0, 0.5),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        }
    }
}

/// Legacy ASCII fallback — the last-resort path when neither the
/// per-species nor per-form PNG layer in `PlantSpriteSet` carries art for
/// `(species, stage)`. Keyed on the coarse `PlantKind` bucket since the
/// existing ASCII templates only cover Tree / BerryBush / Grain shapes.
pub fn get_plant_texture_legacy(
    textures: &EntityTextures,
    kind: PlantKind,
    stage: GrowthStage,
) -> Handle<Image> {
    match kind {
        PlantKind::Tree => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.tree_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.tree_seedling_ascii.clone(),
            _ => textures.tree_mature_ascii.clone(),
        },
        PlantKind::BerryBush => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.plant_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.plant_seedling_ascii.clone(),
            _ => textures.plant_bush_mature_ascii.clone(),
        },
        _ => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.plant_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.plant_seedling_ascii.clone(),
            _ => textures.plant_grain_mature_ascii.clone(),
        },
    }
}

/// Three-tier sprite resolution: species PNG → form PNG → legacy ASCII.
pub fn resolve_plant_sprite(
    plant_sprites: &PlantSpriteSet,
    fallback: &EntityTextures,
    species: PlantSpeciesId,
    form: PlantForm,
    kind: PlantKind,
    stage: GrowthStage,
    variant: u8,
) -> Handle<Image> {
    if species.is_valid() {
        if let Some(slot) = plant_sprites.lookup_species(species, stage) {
            if let Some(h) = slot.handle_for(variant) {
                return h;
            }
        }
    }
    if let Some(slot) = plant_sprites.lookup_form(form, stage) {
        if let Some(h) = slot.handle_for(variant) {
            return h;
        }
    }
    get_plant_texture_legacy(fallback, kind, stage)
}

/// Legacy entry kept as a thin shim for any out-of-tree caller.
pub fn get_plant_texture(
    textures: &EntityTextures,
    kind: PlantKind,
    stage: GrowthStage,
) -> Handle<Image> {
    get_plant_texture_legacy(textures, kind, stage)
}

fn form_of_species(species: PlantSpeciesId, kind: PlantKind) -> PlantForm {
    if species.is_valid() {
        if let Some(def) = crate::simulation::plant_catalog::catalog().def(species) {
            return def.form;
        }
    }
    // Test-fixture fallback: raw `Plant` with no `PlantSpecies`. Pick a
    // form whose `legacy_kind()` matches so per-form PNGs still apply.
    match kind {
        PlantKind::Tree => PlantForm::Tree,
        PlantKind::BerryBush => PlantForm::Shrub,
        PlantKind::Grain => PlantForm::Grass,
    }
}

pub fn spawn_plant_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            &Plant,
            Option<&PlantSpecies>,
            Option<&PlantSpriteVariant>,
        ),
        Without<PlantVisual>,
    >,
    textures: Res<EntityTextures>,
    plant_sprites: Res<PlantSpriteSet>,
) {
    for (entity, plant, species_opt, variant_opt) in query.iter() {
        let species = species_opt.map(|s| s.0).unwrap_or(PlantSpeciesId::NONE);
        let form = form_of_species(species, plant.kind);
        let variant = variant_opt.map(|v| v.0).unwrap_or(0);
        let img = resolve_plant_sprite(
            &plant_sprites,
            &textures,
            species,
            form,
            plant.kind,
            plant.stage,
            variant,
        );
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands
            .entity(entity)
            .insert((PlantVisual, EntityFogState::default(), FogPersistent));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.5),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn update_plant_sprites(
    textures: Res<EntityTextures>,
    plant_sprites: Res<PlantSpriteSet>,
    query: Query<
        (
            &Plant,
            Option<&PlantSpecies>,
            Option<&PlantSpriteVariant>,
            &Children,
        ),
        (With<PlantVisual>, Changed<Plant>),
    >,
    mut sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    for (plant, species_opt, variant_opt, children) in query.iter() {
        let species = species_opt.map(|s| s.0).unwrap_or(PlantSpeciesId::NONE);
        let form = form_of_species(species, plant.kind);
        let variant = variant_opt.map(|v| v.0).unwrap_or(0);
        let img = resolve_plant_sprite(
            &plant_sprites,
            &textures,
            species,
            form,
            plant.kind,
            plant.stage,
            variant,
        );
        for &child in children.iter() {
            if let Ok(mut sprite) = sprites.get_mut(child) {
                sprite.image = img.clone();
            }
        }
    }
}

pub fn animate_person_sprites(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut persons: Query<
        (
            &PersonAI,
            Option<&BiologicalSex>,
            Option<&SkinTone>,
            Option<&HairColor>,
            Option<&ClothingVisual>,
            &Transform,
            &Children,
            &mut FacingDirection,
        ),
        With<Person>,
    >,
    mut child_sprites: Query<(&mut Sprite, &VisualLayer), With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }

    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;

    for (ai, sex_opt, tone_opt, hair_opt, clothing_opt, transform, children, mut facing) in
        persons.iter_mut()
    {
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let diff = target_world - transform.translation.truncate();
        let is_moving = diff.length() > 2.0;

        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 {
                    FacingDirection::East
                } else {
                    FacingDirection::West
                }
            } else {
                if diff.y > 0.0 {
                    FacingDirection::North
                } else {
                    FacingDirection::South
                }
            };
        }

        let is_female = matches!(sex_opt, Some(BiologicalSex::Female));
        let sex_str = if is_female { "female" } else { "male" };
        let tone_str = tone_opt.map(|t| t.as_str()).unwrap_or("tan");
        let hair_str = hair_opt.map(|h| h.as_str()).unwrap_or("brown");
        let cloth_str = clothing_opt.map(|c| c.color_key).unwrap_or("tan");
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };

        let body_key = format!("body_{sex_str}_{tone_str}_{dir}_{frame_str}");
        let hair_key = format!("hair_{sex_str}_{hair_str}_{dir}_{frame_str}");
        let cloth_key = format!("clothing_{sex_str}_{cloth_str}_{dir}_{frame_str}");

        for &child in children.iter() {
            if let Ok((mut sprite, layer)) = child_sprites.get_mut(child) {
                let key = match layer {
                    VisualLayer::Body => body_key.as_str(),
                    VisualLayer::Hair => hair_key.as_str(),
                    VisualLayer::Clothing => cloth_key.as_str(),
                };
                if let Some(img) = sprite_lib.get(key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn animate_wolves_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    animal_tex: Res<AnimalTextures>,
    mut wolves: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Wolf>>,
    mut child_sprites: Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    for (transform, children, mut facing, mut last_pos) in wolves.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        *facing = FacingDirection::from_velocity(diff, *facing);
        last_pos.0 = pos;

        let img = animal_tex.wolf[*facing as usize].clone();
        update_animal_visual(
            &mut child_sprites,
            children,
            img,
            is_moving,
            time.elapsed_secs(),
        );
    }
}

pub fn animate_deer_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    animal_tex: Res<AnimalTextures>,
    mut deer: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Deer>>,
    mut child_sprites: Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    for (transform, children, mut facing, mut last_pos) in deer.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        *facing = FacingDirection::from_velocity(diff, *facing);
        last_pos.0 = pos;

        let img = animal_tex.deer[*facing as usize].clone();
        update_animal_visual(
            &mut child_sprites,
            children,
            img,
            is_moving,
            time.elapsed_secs(),
        );
    }
}

pub fn spawn_horse_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&crate::simulation::reproduction::BiologicalSex>,
        ),
        (With<Horse>, Without<HorseVisual>),
    >,
    sprite_lib: Res<SpriteLibrary>,
    animal_tex: Res<AnimalTextures>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Pixel {
            Some(animal_tex.horse[FacingDirection::South as usize].clone())
        } else {
            sprite_lib.get("creature_horse").cloned()
        };
        let Some(img) = img else { continue };

        let tint = match sex_opt {
            Some(crate::simulation::reproduction::BiologicalSex::Female) => Color::WHITE,
            _ => Color::srgb(0.80, 0.65, 0.45),
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;

        commands.entity(entity).insert((
            HorseVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_horses_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    animal_tex: Res<AnimalTextures>,
    mut horses: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Horse>>,
    mut child_sprites: Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    for (transform, children, mut facing, mut last_pos) in horses.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        *facing = FacingDirection::from_velocity(diff, *facing);
        last_pos.0 = pos;

        let img = animal_tex.horse[*facing as usize].clone();
        update_animal_visual(
            &mut child_sprites,
            children,
            img,
            is_moving,
            time.elapsed_secs(),
        );
    }
}

/// Shared sprite swap + procedural bob/sway helper for PNG-textured animals.
/// Uses sin-wave Y bob (always positive — feet stay grounded) and small Z sway.
fn update_animal_visual(
    child_sprites: &mut Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
    children: &Children,
    img: Handle<Image>,
    is_moving: bool,
    elapsed: f32,
) {
    const BASE_Y: f32 = -8.0;
    const STRIDE_HZ: f32 = 4.0;
    const BOB_AMP: f32 = 1.5;
    const SWAY_AMP_RAD: f32 = 0.0524; // ~3 degrees

    for &child in children.iter() {
        if let Ok((mut sprite, mut tf)) = child_sprites.get_mut(child) {
            if sprite.image != img {
                sprite.image = img.clone();
            }
            if is_moving {
                let phase = elapsed * STRIDE_HZ * std::f32::consts::TAU;
                tf.translation.y = BASE_Y + (phase.sin() * BOB_AMP).abs();
                tf.rotation = Quat::from_rotation_z(phase.cos() * SWAY_AMP_RAD);
            } else {
                tf.translation.y = BASE_Y;
                tf.rotation = Quat::IDENTITY;
            }
        }
    }
}

// ===== Cow / Rabbit / Pig / Fox / Cat sprites =====

pub fn spawn_cow_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Cow>, Without<CowVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
    animal_tex: Res<AnimalTextures>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Pixel {
            Some(animal_tex.cow[FacingDirection::South as usize].clone())
        } else {
            sprite_lib.get("creature_cow").cloned()
        };
        let Some(img) = img else { continue };

        // Cows: females cream, males warm tan
        let tint = match sex_opt {
            Some(BiologicalSex::Female) => Color::srgb(0.95, 0.90, 0.82),
            _ => Color::srgb(0.80, 0.65, 0.50),
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;
        commands.entity(entity).insert((
            CowVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_cows_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    animal_tex: Res<AnimalTextures>,
    mut cows: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Cow>>,
    mut child_sprites: Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    for (transform, children, mut facing, mut last_pos) in cows.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        *facing = FacingDirection::from_velocity(diff, *facing);
        last_pos.0 = pos;

        let img = animal_tex.cow[*facing as usize].clone();
        update_animal_visual(
            &mut child_sprites,
            children,
            img,
            is_moving,
            time.elapsed_secs(),
        );
    }
}

pub fn spawn_rabbit_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Rabbit>, Without<RabbitVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let pixel_key = "anim_rabbit_anim_s_a";
        let ascii_key = "creature_rabbit";
        let img = if *art_mode == ArtMode::Pixel {
            sprite_lib
                .get(pixel_key)
                .cloned()
                .or_else(|| sprite_lib.get(ascii_key).cloned())
        } else {
            sprite_lib.get(ascii_key).cloned()
        };
        let Some(img) = img else { continue };

        let tint = match sex_opt {
            Some(BiologicalSex::Female) => Color::srgb(0.92, 0.88, 0.82),
            _ => Color::srgb(0.78, 0.72, 0.65),
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;
        commands.entity(entity).insert((
            RabbitVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_rabbits_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut rabbits: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Rabbit>>,
    mut child_sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;
    for (transform, children, mut facing, mut last_pos) in rabbits.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 {
                    FacingDirection::East
                } else {
                    FacingDirection::West
                }
            } else {
                if diff.y > 0.0 {
                    FacingDirection::North
                } else {
                    FacingDirection::South
                }
            };
        }
        last_pos.0 = pos;
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };
        let key = format!("anim_rabbit_anim_{dir}_{frame_str}");
        for &child in children.iter() {
            if let Ok(mut sprite) = child_sprites.get_mut(child) {
                if let Some(img) = sprite_lib.get(&key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn spawn_pig_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Pig>, Without<PigVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let pixel_key = "anim_pig_s_a";
        let ascii_key = "creature_pig";
        let img = if *art_mode == ArtMode::Pixel {
            sprite_lib
                .get(pixel_key)
                .cloned()
                .or_else(|| sprite_lib.get(ascii_key).cloned())
        } else {
            sprite_lib.get(ascii_key).cloned()
        };
        let Some(img) = img else { continue };

        let tint = match sex_opt {
            Some(BiologicalSex::Female) => Color::srgb(0.95, 0.78, 0.75),
            _ => Color::srgb(0.85, 0.62, 0.58),
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;
        commands.entity(entity).insert((
            PigVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_pigs_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut pigs: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Pig>>,
    mut child_sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;
    for (transform, children, mut facing, mut last_pos) in pigs.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 {
                    FacingDirection::East
                } else {
                    FacingDirection::West
                }
            } else {
                if diff.y > 0.0 {
                    FacingDirection::North
                } else {
                    FacingDirection::South
                }
            };
        }
        last_pos.0 = pos;
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };
        let key = format!("anim_pig_{dir}_{frame_str}");
        for &child in children.iter() {
            if let Ok(mut sprite) = child_sprites.get_mut(child) {
                if let Some(img) = sprite_lib.get(&key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn spawn_fox_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Fox>, Without<FoxVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let pixel_key = "anim_fox_s_a";
        let ascii_key = "creature_fox";
        let img = if *art_mode == ArtMode::Pixel {
            sprite_lib
                .get(pixel_key)
                .cloned()
                .or_else(|| sprite_lib.get(ascii_key).cloned())
        } else {
            sprite_lib.get(ascii_key).cloned()
        };
        let Some(img) = img else { continue };

        let tint = match sex_opt {
            Some(BiologicalSex::Female) => Color::srgb(0.95, 0.65, 0.42),
            _ => Color::srgb(0.85, 0.50, 0.30),
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;
        commands.entity(entity).insert((
            FoxVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_foxes_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut foxes: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Fox>>,
    mut child_sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;
    for (transform, children, mut facing, mut last_pos) in foxes.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 {
                    FacingDirection::East
                } else {
                    FacingDirection::West
                }
            } else {
                if diff.y > 0.0 {
                    FacingDirection::North
                } else {
                    FacingDirection::South
                }
            };
        }
        last_pos.0 = pos;
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };
        let key = format!("anim_fox_{dir}_{frame_str}");
        for &child in children.iter() {
            if let Ok(mut sprite) = child_sprites.get_mut(child) {
                if let Some(img) = sprite_lib.get(&key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn spawn_cat_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Cat>, Without<CatVisual>)>,
    sprite_lib: Res<SpriteLibrary>,
    animal_tex: Res<AnimalTextures>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Pixel {
            Some(animal_tex.cat[FacingDirection::South as usize].clone())
        } else {
            sprite_lib.get("creature_cat").cloned()
        };
        let Some(img) = img else { continue };

        let tint = match sex_opt {
            Some(BiologicalSex::Female) => Color::srgb(0.85, 0.78, 0.70),
            _ => Color::srgb(0.55, 0.45, 0.38),
        };
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;
        commands.entity(entity).insert((
            CatVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn animate_cats_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    animal_tex: Res<AnimalTextures>,
    mut cats: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Cat>>,
    mut child_sprites: Query<
        (&mut Sprite, &mut Transform),
        (
            With<VisualChild>,
            Without<Wolf>,
            Without<Deer>,
            Without<Horse>,
            Without<Cow>,
            Without<Cat>,
        ),
    >,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    for (transform, children, mut facing, mut last_pos) in cats.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        *facing = FacingDirection::from_velocity(diff, *facing);
        last_pos.0 = pos;

        let img = animal_tex.cat[*facing as usize].clone();
        update_animal_visual(
            &mut child_sprites,
            children,
            img,
            is_moving,
            time.elapsed_secs(),
        );
    }
}

/// Hide entities that don't belong on the layer the camera is viewing.
/// Surface mode (CameraViewZ == i32::MAX): show entities whose Z equals
/// the surface_z of their tile (i.e. above-ground entities). Underground
/// mode: show only entities whose Z equals camera_view_z.
/// Entities marked `FogPersistent` (static structures) remain visible at a dim
/// tint in explored-but-not-currently-visible tiles. All other fog-tracked
/// entities (persons, animals) hide entirely outside currently-visible tiles.
pub fn update_entity_z_visibility_system(
    camera_view_z: Res<crate::rendering::camera::CameraViewZ>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    fog_map: Res<crate::rendering::fog::FogMap>,
    mut q: Query<
        (
            &Transform,
            &mut Visibility,
            &mut EntityFogState,
            Option<&PersonAI>,
            Has<Person>,
            Has<FogPersistent>,
        ),
        With<EntityFogState>,
    >,
) {
    use crate::world::terrain::TILE_SIZE;
    let cam_z = camera_view_z.0;
    for (transform, mut vis, mut fog_state, person_ai, is_person, fog_persistent) in q.iter_mut() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let surf_z = chunk_map.surface_z_at(tx, ty);
        let entity_z = match person_ai {
            Some(ai) if is_person => ai.current_z as i32,
            _ => surf_z,
        };
        let should_show = if cam_z == i32::MAX {
            entity_z == surf_z
        } else {
            entity_z == cam_z
        };
        let fog_visible = fog_map.is_visible((tx as i32, ty as i32));
        let fog_explored = fog_map.is_explored((tx as i32, ty as i32));
        let fog_ok = fog_visible || (fog_persistent && fog_explored);
        let new_vis = if should_show && fog_ok {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *vis != new_vis {
            *vis = new_vis;
        }
        if should_show {
            let new_fog_state = if fog_visible {
                EntityFogState::Visible
            } else {
                EntityFogState::Explored
            };
            if *fog_state != new_fog_state {
                *fog_state = new_fog_state;
            }
        }
    }
}

/// Single source of truth for sprite.color on every VisualChild. Combines:
///   - fog factor (Visible / Explored)
///   - AnimalSexTint base (animals only)
///   - ClothingVisual visibility (sets clothing layer alpha to 0 when hidden)
///   - CombatAnimations hit-flash (red tint while hit_timer > 0)
/// No other system should write sprite.color on VisualChild — that causes flicker
/// and layer-to-layer color mismatches.
pub fn apply_entity_fog_tint_system(
    entities: Query<
        (
            &Visibility,
            &EntityFogState,
            &Children,
            Option<&ClothingVisual>,
            Option<&crate::rendering::animations::CombatAnimations>,
        ),
        With<EntityFogState>,
    >,
    mut child_sprites: Query<
        (
            &mut Sprite,
            Option<&AnimalSexTint>,
            Option<&VehicleCellTint>,
            Option<&VisualLayer>,
        ),
        With<VisualChild>,
    >,
) {
    for (vis, fog_state, children, clothing_opt, anim_opt) in entities.iter() {
        if *vis == Visibility::Hidden {
            continue;
        }
        let fog_factor = if *fog_state == EntityFogState::Visible {
            1.0f32
        } else {
            0.35
        };
        let clothing_visible = clothing_opt.map(|c| c.visible).unwrap_or(true);
        let hit_flash = anim_opt.map(|a| a.hit_timer > 0.0).unwrap_or(false);

        for &child in children.iter() {
            if let Ok((mut sprite, sex_tint, vehicle_tint, layer_opt)) = child_sprites.get_mut(child) {
                // Per-cell vehicle tint takes precedence over the
                // sex-tint base; otherwise white.
                let base = vehicle_tint
                    .map(|t| t.0.to_srgba())
                    .or_else(|| sex_tint.map(|t| t.0.to_srgba()))
                    .unwrap_or(bevy::color::Srgba::WHITE);
                let (mut r, mut g, mut b) = (base.red, base.green, base.blue);
                if hit_flash {
                    r = 1.0;
                    g = 0.4;
                    b = 0.4;
                }
                let alpha = if matches!(layer_opt, Some(VisualLayer::Clothing)) && !clothing_visible
                {
                    0.0
                } else {
                    1.0
                };
                let new_color = Color::srgba(r * fog_factor, g * fog_factor, b * fog_factor, alpha);
                if sprite.color != new_color {
                    sprite.color = new_color;
                }
            }
        }
    }
}

pub fn toggle_art_mode(keyboard: Res<ButtonInput<KeyCode>>, mut art_mode: ResMut<ArtMode>) {
    if keyboard.just_pressed(KeyCode::KeyT) {
        *art_mode = match *art_mode {
            ArtMode::Ascii => ArtMode::Pixel,
            ArtMode::Pixel => ArtMode::Ascii,
        };
        info!("Art Mode changed to: {:?}", *art_mode);
    }
}
pub fn handle_art_mode_change(
    mut commands: Commands,
    art_mode: Res<ArtMode>,
    people: Query<Entity, With<PersonVisual>>,
    wolves: Query<Entity, With<WolfVisual>>,
    deer: Query<Entity, With<DeerVisual>>,
    horses: Query<Entity, With<HorseVisual>>,
    new_animals: Query<
        Entity,
        bevy::prelude::Or<(
            With<CowVisual>,
            With<RabbitVisual>,
            With<PigVisual>,
            With<FoxVisual>,
            With<CatVisual>,
        )>,
    >,
    walls: Query<Entity, With<WallVisual>>,
    beds: Query<Entity, With<BedVisual>>,
    centers: Query<Entity, With<FactionCenterVisual>>,
    plants: Query<Entity, With<PlantVisual>>,
    blueprints: Query<Entity, With<BlueprintVisual>>,
    children: Query<(Entity, &Children)>,
) {
    if art_mode.is_changed() && !art_mode.is_added() {
        let all_visuals = people
            .iter()
            .chain(wolves.iter())
            .chain(deer.iter())
            .chain(horses.iter())
            .chain(new_animals.iter())
            .chain(walls.iter())
            .chain(beds.iter())
            .chain(centers.iter())
            .chain(plants.iter())
            .chain(blueprints.iter());

        for entity in all_visuals {
            if let Ok((_, children)) = children.get(entity) {
                for &child in children.iter() {
                    // Only despawn if it's a visual child to avoid destroying actual game logic children if any
                    commands.entity(child).despawn_recursive();
                }
            }
            commands.entity(entity).remove::<(
                PersonVisual,
                WolfVisual,
                DeerVisual,
                HorseVisual,
                CowVisual,
                RabbitVisual,
                PigVisual,
                FoxVisual,
                CatVisual,
                WallVisual,
                BedVisual,
                FactionCenterVisual,
                PlantVisual,
                BlueprintVisual,
            )>();
        }
    }
}

/// Updates ClothingVisual color key whenever a person's Equipment changes.
/// The animate system picks up the new key on the next frame automatically.
pub fn update_clothing_from_equipment(
    mut persons: Query<
        (&Equipment, Option<&mut ClothingVisual>),
        (With<Person>, Changed<Equipment>),
    >,
) {
    for (equip, clothing_opt) in &mut persons {
        if let Some(mut clothing) = clothing_opt {
            let has_armor = equip.items.contains_key(&EquipmentSlot::TorsoArmor);
            clothing.visible = has_armor;
            clothing.color_key = if has_armor { "grey" } else { "tan" };
        }
    }
}

pub fn spawn_blueprint_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Blueprint), (With<Blueprint>, Without<BlueprintVisual>)>,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
) {
    for (entity, bp) in query.iter() {
        let scaffold_img = textures.blueprint_ascii.clone();

        let mut scaffold_sprite = Sprite::from_image(scaffold_img);
        scaffold_sprite.anchor = Anchor::BottomCenter;

        let ghost_img = match bp.kind {
            BuildSiteKind::Wall(WallMaterial::Palisade) => textures.wall_palisade_ascii.clone(),
            BuildSiteKind::Wall(WallMaterial::WattleDaub) => textures.wall_wattle_ascii.clone(),
            BuildSiteKind::Wall(WallMaterial::Stone) => textures.wall_stone_ascii.clone(),
            BuildSiteKind::Wall(WallMaterial::Mudbrick) => textures.wall_mudbrick_ascii.clone(),
            BuildSiteKind::Wall(WallMaterial::CutStone) => textures.wall_cutstone_ascii.clone(),
            BuildSiteKind::Door => textures.door_ascii.clone(),
            // Thin housing wall/door ghosts reuse the whole-tile art for now;
            // Phase 5 ships edge-positioned, orientation-aware sprites.
            BuildSiteKind::EdgeWall(WallMaterial::Palisade) => textures.wall_palisade_ascii.clone(),
            BuildSiteKind::EdgeWall(WallMaterial::WattleDaub) => textures.wall_wattle_ascii.clone(),
            BuildSiteKind::EdgeWall(WallMaterial::Stone) => textures.wall_stone_ascii.clone(),
            BuildSiteKind::EdgeWall(WallMaterial::Mudbrick) => textures.wall_mudbrick_ascii.clone(),
            BuildSiteKind::EdgeWall(WallMaterial::CutStone) => textures.wall_cutstone_ascii.clone(),
            BuildSiteKind::EdgeDoor => textures.door_ascii.clone(),
            BuildSiteKind::Bed => textures.bed_ascii.clone(),
            // Bedroll reuses the bed sprite for now — Phase 9 ships proper
            // procedural pixel art for nomadic kit.
            BuildSiteKind::Bedroll => textures.bed_ascii.clone(),
            BuildSiteKind::Tent => sprite_lib
                .get("building_tent")
                .cloned()
                .unwrap_or_else(|| textures.wall_palisade_ascii.clone()),
            BuildSiteKind::Yurt => sprite_lib
                .get("building_yurt")
                .cloned()
                .unwrap_or_else(|| textures.wall_palisade_ascii.clone()),
            BuildSiteKind::Campfire => textures.campfire_ascii.clone(),
            BuildSiteKind::Workbench => textures.workbench_ascii.clone(),
            BuildSiteKind::Loom => textures.loom_ascii.clone(),
            BuildSiteKind::Table => textures.table_ascii.clone(),
            BuildSiteKind::Chair => textures.chair_ascii.clone(),
            // Stub: reuse closest existing sprite until proper art ships.
            BuildSiteKind::Granary => textures.workbench_ascii.clone(),
            BuildSiteKind::Shrine => textures.wall_stone_ascii.clone(),
            BuildSiteKind::Market => textures.table_ascii.clone(),
            BuildSiteKind::Barracks => textures.wall_stone_ascii.clone(),
            BuildSiteKind::Monument => textures.wall_cutstone_ascii.clone(),
            // Stub: reuse the wall_stone sprite until dedicated latrine art ships.
            BuildSiteKind::Latrine => textures.wall_stone_ascii.clone(),
            // Stub: reuse table sprite (planks-over-water suggestion) until
            // a dedicated bridge blueprint sprite ships.
            BuildSiteKind::Bridge => textures.table_ascii.clone(),
            // Stub: reuse the stone-wall sprite — a dam reads as masonry,
            // not a timber deck — until dedicated dam art ships.
            BuildSiteKind::Dam => textures.wall_stone_ascii.clone(),
            BuildSiteKind::Well => sprite_lib
                .get("building_well")
                .cloned()
                .unwrap_or_else(|| textures.wall_stone_ascii.clone()),
            // Husbandry kit reuses palisade (fence-like) until dedicated art
            // ships. FeedTrough/HitchingPost are tiny so the table sprite
            // reads better.
            BuildSiteKind::Pen | BuildSiteKind::Stable => textures.wall_palisade_ascii.clone(),
            BuildSiteKind::FeedTrough | BuildSiteKind::HitchingPost => textures.table_ascii.clone(),
            // Stub: a vehicle yard reads as a timber work-area — reuse the
            // workbench sprite until dedicated art ships.
            BuildSiteKind::VehicleYard => textures.workbench_ascii.clone(),
            // Poor-housing primitives: a flat mat (not a bed frame) and a
            // lean-to windbreak (not a wall). Fall back to the closest legacy
            // sprite if the procedural art isn't registered.
            BuildSiteKind::SleepingMat(_) => sprite_lib
                .get("building_sleeping_mat")
                .cloned()
                .unwrap_or_else(|| textures.bed_ascii.clone()),
            BuildSiteKind::LightShelter(_) => sprite_lib
                .get("building_lean_to")
                .cloned()
                .unwrap_or_else(|| textures.wall_palisade_ascii.clone()),
        };

        let mut ghost_sprite = Sprite::from_image(ghost_img);
        ghost_sprite.anchor = Anchor::BottomCenter;
        ghost_sprite.color = Color::srgba(1.0, 1.0, 1.0, 0.4);

        commands
            .entity(entity)
            .insert((BlueprintVisual, EntityFogState::default(), FogPersistent))
            .with_children(|parent| {
                parent.spawn((
                    VisualChild,
                    scaffold_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.2),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));

                parent.spawn((
                    VisualChild,
                    ghost_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.1),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
    }
}
