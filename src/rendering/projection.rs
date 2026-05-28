//! Tilted-view projection layer.
//!
//! `MapViewMode::TopDown` is the bit-exact identity fallback; `Tilted`
//! compresses Y and lifts by elevation, producing a 2.5D oblique view.
//!
//! Projection runs symmetrically: `revert_view_projection_system`
//! (PreUpdate) restores `Transform.translation` to the logical position so
//! simulation systems see top-down coords; `apply_view_projection_system`
//! (PostUpdate, after sim) re-projects for the renderer. In TopDown mode
//! both passes early-return — the system list is a no-op.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

use crate::world::chunk::ChunkMap;
use crate::world::terrain::{tile_to_world, world_to_tile, TILE_SIZE};

/// Bundled `MapViewMode + MapProjection` resource handles. Most call sites
/// (picking, drag-select, camera focus jumps) need both together; using this
/// SystemParam keeps callers under Bevy's per-system 16-param ceiling.
#[derive(SystemParam)]
pub struct ViewProjection<'w> {
    pub mode: Res<'w, MapViewMode>,
    pub proj: Res<'w, MapProjection>,
}

impl<'w> ViewProjection<'w> {
    /// Inverse-project a world cursor onto a flat plane at elevation
    /// `plane_z` and return the logical tile.
    #[inline]
    pub fn unproject_tile(&self, view: Vec2, plane_z: i8) -> (i32, i32) {
        unproject_to_tile(view, plane_z, *self.mode, &self.proj)
    }

    /// Camera-position helper — view-space → logical world-space.
    #[inline]
    pub fn camera_to_logical(&self, camera: Vec2) -> Vec2 {
        camera_view_to_logical(camera, *self.mode, &self.proj)
    }

    /// Camera-position helper — logical world-space → view-space.
    #[inline]
    pub fn logical_to_camera(&self, logical: Vec2) -> Vec2 {
        logical_to_view_camera(logical, *self.mode, &self.proj)
    }

    #[inline]
    pub fn is_tilted(&self) -> bool {
        *self.mode == MapViewMode::Tilted
    }
}

/// Bundle for gizmo / debug overlay drawing in projected coords. Wraps
/// `ViewProjection + ChunkMap` so callers can map a logical world position
/// (the same coord-space `tile_to_world` produces) into view-space ready
/// for `gizmos.line_2d` / `circle_2d` / `rect_2d`.
#[derive(SystemParam)]
pub struct LogicalProjector<'w> {
    pub view_projection: ViewProjection<'w>,
    pub chunk_map: Res<'w, ChunkMap>,
}

impl<'w> LogicalProjector<'w> {
    /// Map a logical world position (e.g. `tile_to_world(tx, ty)` or a
    /// person's logical Transform.translation) into view-space — the
    /// coordinate `gizmos.*_2d` expects so the gizmo lines up with the
    /// rendered (tilted) tile beneath it.
    #[inline]
    pub fn project(&self, logical: Vec2) -> Vec2 {
        if *self.view_projection.mode == MapViewMode::TopDown {
            return logical;
        }
        let tx = (logical.x / TILE_SIZE).floor() as i32;
        let ty = (logical.y / TILE_SIZE).floor() as i32;
        let elev_z_i32 = self.chunk_map.surface_z_at(tx, ty);
        let elev_z = if elev_z_i32 >= crate::world::chunk::Z_MIN {
            elev_z_i32.clamp(i8::MIN as i32, i8::MAX as i32) as i8
        } else {
            0
        };
        let (dy, _dz) = project_delta(
            logical.y,
            elev_z,
            *self.view_projection.mode,
            &self.view_projection.proj,
        );
        Vec2::new(logical.x, logical.y + dy)
    }
}

/// Bundle for cursor-driven picking. Combines the cursor + camera lookup
/// with the projection state so callers (hover / right-click / drag-select)
/// can resolve a screen cursor to the correct logical tile in either mode
/// — and stay under Bevy's 16-param-per-system ceiling.
#[derive(SystemParam)]
pub struct CursorParams<'w, 's> {
    pub contexts: EguiContexts<'w, 's>,
    pub windows: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    pub camera_query: Query<'w, 's, (&'static Camera, &'static GlobalTransform), With<Camera>>,
    pub view_projection: ViewProjection<'w>,
    pub chunk_map: Res<'w, ChunkMap>,
}

/// Result of picking the cursor against the tile grid. `world_view` is the
/// raw cursor in view-space (what `viewport_to_world_2d` returns); `tile`
/// is the elevation-refined logical tile under the cursor. In TopDown
/// mode `world_view == world_logical`. `screen_pos` is the raw window
/// cursor in pixels (for placing egui popups at the click site).
#[derive(Clone, Copy, Debug)]
pub struct CursorPick {
    pub screen_pos: Vec2,
    pub world_view: Vec2,
    pub world_logical: Vec2,
    pub tile: (i32, i32),
}

impl<'w, 's> CursorParams<'w, 's> {
    /// True if the cursor is positioned over an egui panel (and so should
    /// not drive world picking). Already factored in by `cursor_pick`.
    pub fn egui_owns_pointer(&mut self) -> bool {
        let ctx = self.contexts.ctx_mut();
        ctx.is_pointer_over_area() || ctx.wants_pointer_input()
    }

    /// Read cursor → camera → world → logical tile, with one elevation
    /// refinement pass so cliff-top tiles resolve correctly in tilted mode.
    /// Returns `None` if the cursor is outside the window or owned by egui.
    pub fn cursor_pick(&mut self) -> Option<CursorPick> {
        if self.egui_owns_pointer() {
            return None;
        }
        let window = self.windows.get_single().ok()?;
        let (camera, cam_transform) = self.camera_query.get_single().ok()?;
        let cursor_pos = window.cursor_position()?;
        let world_view = camera
            .viewport_to_world_2d(cam_transform, cursor_pos)
            .ok()?;

        // Tilted-mode cliff picking: for each candidate elevation z in
        // [Z_MIN, Z_MAX], compute the logical tile that would project to
        // exactly `world_view` if its surface_z were that z. A candidate
        // *matches* when the actual `chunk_map.surface_z_at(tx, ty)` equals
        // the assumed elevation — meaning that specific tile genuinely
        // projects under the cursor. When multiple candidates match (e.g.
        // a stack of cliffs along the camera-facing axis), pick the one
        // closest to the camera (largest tile_y, since south-facing
        // cliffs hide what's behind them). In TopDown mode the loop
        // collapses to a single iteration at z=0.
        let (tile, plane_z) = self.pick_cliff_aware(world_view);
        let world_logical = unproject_to_world(
            world_view,
            plane_z,
            *self.view_projection.mode,
            &self.view_projection.proj,
        );

        Some(CursorPick {
            screen_pos: cursor_pos,
            world_view,
            world_logical,
            tile,
        })
    }

    /// Convenience: just the logical tile, ignoring the world coords.
    pub fn cursor_tile(&mut self) -> Option<(i32, i32)> {
        self.cursor_pick().map(|p| p.tile)
    }

    /// Cliff-aware picking shared by `cursor_pick`. Returns
    /// `((tile_x, tile_y), plane_z)` where `plane_z` is the elevation
    /// the cursor was resolved against (used by the caller to recover
    /// `world_logical`).
    fn pick_cliff_aware(&self, world_view: Vec2) -> ((i32, i32), i8) {
        let mode = *self.view_projection.mode;
        // TopDown: identity unproject onto z=0 plane, no walk needed.
        if mode == MapViewMode::TopDown {
            return (self.view_projection.unproject_tile(world_view, 0), 0);
        }
        let proj = &self.view_projection.proj;
        let tx = (world_view.x / TILE_SIZE).floor() as i32;
        // Walk every elevation band — only ~32 candidates total. The first
        // matching candidate found while iterating high→low elevation will
        // also have the largest projected y (taller tiles project higher),
        // so we still need the explicit "closest tile_y to camera" tie-break
        // when the surface dips back up further north.
        let mut best: Option<((i32, i32), i8)> = None;
        for elev in (crate::world::chunk::Z_MIN..=crate::world::chunk::Z_MAX).rev() {
            let elev_i8 = elev as i8;
            let logical_y = (world_view.y - elev as f32 * proj.z_step_px) / proj.y_scale;
            let ty = (logical_y / TILE_SIZE).floor() as i32;
            let actual_z = self.chunk_map.surface_z_at(tx, ty);
            if actual_z != elev as i32 {
                continue;
            }
            // Match. Keep the candidate whose tile_y is largest (closest to
            // the camera in our south-facing oblique projection).
            match best {
                None => best = Some(((tx, ty), elev_i8)),
                Some(((_, prev_ty), _)) if ty > prev_ty => {
                    best = Some(((tx, ty), elev_i8));
                }
                _ => {}
            }
        }
        if let Some(b) = best {
            return b;
        }
        // No exact-elevation match (e.g. cursor over an unloaded chunk).
        // Fall back to single-step refinement so callers still get a tile
        // — the same path the function used pre-walk.
        let (tx0, ty0) = self.view_projection.unproject_tile(world_view, 0);
        let surf_z = self.chunk_map.surface_z_at(tx0, ty0);
        let plane_z = if surf_z >= crate::world::chunk::Z_MIN {
            surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8
        } else {
            0
        };
        (
            self.view_projection.unproject_tile(world_view, plane_z),
            plane_z,
        )
    }
}

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MapViewMode {
    #[default]
    TopDown,
    Tilted,
}

impl MapViewMode {
    pub fn label(self) -> &'static str {
        match self {
            MapViewMode::TopDown => "View: Top",
            MapViewMode::Tilted => "View: Tilt",
        }
    }
    pub fn toggled(self) -> Self {
        match self {
            MapViewMode::TopDown => MapViewMode::Tilted,
            MapViewMode::Tilted => MapViewMode::TopDown,
        }
    }
}

#[derive(Resource, Clone, Copy, Debug)]
pub struct MapProjection {
    pub y_scale: f32,
    pub z_step_px: f32,
    pub pitch_deg: f32,
}

impl Default for MapProjection {
    fn default() -> Self {
        Self {
            y_scale: 0.58,
            // Bumped from 3.0 — at 3 px / Z a single-step cliff is almost
            // invisible against Y-compressed neighbours. 6 px makes 1 Z
            // unit clearly readable while keeping the full -16..+15 range
            // (~186 px lift) inside the camera's visual budget.
            z_step_px: 6.0,
            pitch_deg: 54.0,
        }
    }
}

/// Marker + elevation for entities that should follow the projection.
///
/// `Static` is used by tile sprites, walls, beds, plants, etc. — entities
/// whose tile and surface elevation are fixed at spawn. `Dynamic` is used by
/// mobile agents (people, animals) — elevation is looked up from the
/// `ChunkMap` at projection time based on the current tile under the
/// entity's logical translation.
#[derive(Component, Clone, Copy, Debug)]
pub enum ProjectedAnchor {
    Static { z: i8 },
    Dynamic,
}

/// Per-entity projection bookkeeping. Stores the delta most recently added by
/// `apply_view_projection_system` so `revert_view_projection_system` can
/// subtract the same amount on the next frame, regardless of mode changes.
#[derive(Component, Default, Clone, Copy, Debug)]
pub struct ProjectionState {
    pub last_dy: f32,
    pub last_dz: f32,
}

/// Sprite quad rendered between a tile and its lower south neighbour to
/// read as a vertical cliff face under tilted projection. Anchored top-
/// centre so its `custom_size.y` extends downward from the south edge of
/// the upper tile to the top edge of the lower neighbour.
///
/// Spawned at chunk-load time in `world::chunk_streaming::spawn_chunk_sprites`
/// (one per cliff edge, never on flat terrain). Its own
/// `ProjectedAnchor::Static { z: south_z }` makes it project with the
/// *lower* tile so the sprite's top edge lands exactly where the upper
/// tile's projected bottom does. `update_skirt_visibility_system` toggles
/// visibility per `MapViewMode` (hidden in TopDown).
#[derive(Component, Clone, Copy, Debug)]
pub struct ElevationSkirt {
    pub delta_z: u8,
}

/// Compute (dy, dz) to add to `Transform.translation` to convert a logical
/// position into projected screen-space. In `TopDown` mode this is always
/// `(0, 0)` — the projection layer is a no-op identity.
#[inline]
pub fn project_delta(
    logical_y: f32,
    elev_z: i8,
    mode: MapViewMode,
    proj: &MapProjection,
) -> (f32, f32) {
    // y-sort dz applies in both modes — keeps overlapping bottom-anchored
    // sprites in consistent screen-front-to-back order. Scale chosen so
    // two adjacent rows differ by ~0.0016 z (smaller than kind-bias gaps
    // of 0.1) but accumulates noticeably across a visible screen
    // (~50 rows → ~0.08 z). Walls, plants and ground items therefore
    // y-sort within their layer; cross-layer ordering still respects
    // kind_bias.
    let dz = -logical_y * 0.0001 - (elev_z as f32) * 0.0005;
    let dy = match mode {
        MapViewMode::TopDown => 0.0,
        // Tilted compresses Y by `y_scale` and lifts by elevation in pixels.
        MapViewMode::Tilted => logical_y * (proj.y_scale - 1.0) + elev_z as f32 * proj.z_step_px,
    };
    (dy, dz)
}

/// Convenience wrapper for callers that already have a logical `Vec3`.
#[inline]
pub fn project(logical: Vec3, elev_z: i8, mode: MapViewMode, proj: &MapProjection) -> Vec3 {
    let (dy, dz) = project_delta(logical.y, elev_z, mode, proj);
    Vec3::new(logical.x, logical.y + dy, logical.z + dz)
}

/// Inverse of `project()` onto a flat plane at elevation `plane_z`.
/// Cliffs/elevation refinement on top of this is not yet implemented — for
/// callers that need accurate picking near cliffs in Tilted mode, follow up
/// with the bounded walk described in `plans/tilt-view.md::Picking`.
#[inline]
pub fn unproject_to_world(
    view: Vec2,
    plane_z: i8,
    mode: MapViewMode,
    proj: &MapProjection,
) -> Vec2 {
    match mode {
        MapViewMode::TopDown => view,
        MapViewMode::Tilted => {
            // Invert: view_y = logical_y * y_scale + plane_z * z_step_px
            // => logical_y = (view_y - plane_z * z_step_px) / y_scale
            let logical_y = (view.y - plane_z as f32 * proj.z_step_px) / proj.y_scale;
            Vec2::new(view.x, logical_y)
        }
    }
}

/// Combined helper: invert from screen-world to a logical tile.
#[inline]
pub fn unproject_to_tile(
    view: Vec2,
    plane_z: i8,
    mode: MapViewMode,
    proj: &MapProjection,
) -> (i32, i32) {
    world_to_tile(unproject_to_world(view, plane_z, mode, proj))
}

/// Camera helpers — convert between camera-space (projected) and the logical
/// tile-grid coordinate the camera is centred on. Camera is treated as
/// anchored at `plane_z = 0` (no elevation lift).
#[inline]
pub fn camera_view_to_logical(camera: Vec2, mode: MapViewMode, proj: &MapProjection) -> Vec2 {
    unproject_to_world(camera, 0, mode, proj)
}

/// Inverse of `camera_view_to_logical` — given a logical position, where
/// should the camera be translated so that position appears centred?
#[inline]
pub fn logical_to_view_camera(logical: Vec2, mode: MapViewMode, proj: &MapProjection) -> Vec2 {
    match mode {
        MapViewMode::TopDown => logical,
        MapViewMode::Tilted => Vec2::new(logical.x, logical.y * proj.y_scale),
    }
}

/// Convert a logical tile to the camera-space (projected) position whose
/// centre is that tile.
#[inline]
pub fn tile_to_view_camera(
    tile_x: i32,
    tile_y: i32,
    mode: MapViewMode,
    proj: &MapProjection,
) -> Vec2 {
    logical_to_view_camera(tile_to_world(tile_x, tile_y), mode, proj)
}

/// PreUpdate: undo the projection delta last applied to each anchored
/// entity, so simulation systems in FixedUpdate / Update see logical
/// (top-down) `Transform.translation` values.
pub fn revert_view_projection_system(
    _mode: Res<MapViewMode>,
    mut q: Query<(&mut Transform, &mut ProjectionState), With<ProjectedAnchor>>,
) {
    // Both modes write a delta now: Tilted writes dy + dz; TopDown writes
    // a dz-only y-sort offset so overlapping bottom-anchored sprites stack
    // by screen position rather than entity-spawn order.
    for (mut tf, mut state) in &mut q {
        if state.last_dy != 0.0 {
            tf.translation.y -= state.last_dy;
        }
        if state.last_dz != 0.0 {
            tf.translation.z -= state.last_dz;
        }
        state.last_dy = 0.0;
        state.last_dz = 0.0;
    }
}

/// PostUpdate: re-apply the projection so the renderer sees tilted
/// positions. Iterates in TopDown too so that any newly-spawned entity gets
/// its `ProjectionState` cleanly initialised — but the work per entity is
/// just a delta-zero check, no Transform mutation.
pub fn apply_view_projection_system(
    mode: Res<MapViewMode>,
    proj: Res<MapProjection>,
    chunk_map: Res<ChunkMap>,
    mut q: Query<(&mut Transform, &ProjectedAnchor, &mut ProjectionState)>,
) {
    for (mut tf, anchor, mut state) in &mut q {
        let elev_z = match *anchor {
            ProjectedAnchor::Static { z } => z,
            ProjectedAnchor::Dynamic => {
                let tx = (tf.translation.x / TILE_SIZE).floor() as i32;
                let ty = (tf.translation.y / TILE_SIZE).floor() as i32;
                let z = chunk_map.surface_z_at(tx, ty);
                if z < i8::MIN as i32 {
                    0
                } else {
                    z.clamp(i8::MIN as i32, i8::MAX as i32) as i8
                }
            }
        };
        let (dy, dz) = project_delta(tf.translation.y, elev_z, *mode, &proj);
        if dy != 0.0 {
            tf.translation.y += dy;
        }
        if dz != 0.0 {
            tf.translation.z += dz;
        }
        state.last_dy = dy;
        state.last_dz = dz;
    }
}

/// Helper used by chunk-load to size a freshly-spawned skirt sprite. Caller
/// is responsible for the rest of the entity (Sprite color, Transform with
/// `ProjectedAnchor::Static { z: south_z }`, the marker `ElevationSkirt`).
#[inline]
pub fn skirt_size(delta_z: u8, proj: &MapProjection) -> Vec2 {
    Vec2::new(TILE_SIZE, delta_z as f32 * proj.z_step_px)
}

/// Build the skirt sprite ready to be spawned. Caller adds Transform +
/// ProjectedAnchor based on the cliff geometry.
pub fn skirt_sprite(delta_z: u8, proj: &MapProjection, mode: MapViewMode) -> Sprite {
    // In TopDown the skirt would wedge between two stacked tiles and read
    // as a horizontal seam — squash to zero size so it's invisible even
    // before `update_skirt_visibility_system` flips Visibility off.
    let size = if mode == MapViewMode::TopDown {
        Vec2::ZERO
    } else {
        skirt_size(delta_z, proj)
    };
    Sprite {
        color: Color::srgba(0.06, 0.05, 0.10, 0.92),
        custom_size: Some(size),
        anchor: Anchor::TopCenter,
        ..default()
    }
}

/// Resize each `ElevationSkirt` according to the current `MapProjection` and
/// hide skirts in TopDown mode. Cheap — touches only the skirt entities,
/// not every tile sprite.
pub fn update_skirt_visibility_system(
    mode: Res<MapViewMode>,
    proj: Res<MapProjection>,
    mut q: Query<(&ElevationSkirt, &mut Sprite, &mut Visibility)>,
) {
    let tilted = *mode == MapViewMode::Tilted;
    for (skirt, mut sprite, mut vis) in &mut q {
        if tilted {
            sprite.custom_size = Some(skirt_size(skirt.delta_z, &proj));
            *vis = Visibility::Inherited;
        } else {
            *vis = Visibility::Hidden;
        }
    }
}

/// Generic auto-attach: any entity carrying marker `T` and lacking
/// `ProjectedAnchor` gets a `Dynamic` anchor (elevation looked up from
/// `ChunkMap` at projection time). Lets us roll out the projection layer
/// without touching every spawn site individually — register one
/// `auto_attach_dynamic::<T>` per world-living marker type.
pub fn auto_attach_dynamic<T: Component>(
    mut commands: Commands,
    q: Query<Entity, (With<T>, Without<ProjectedAnchor>)>,
) {
    for e in &q {
        commands
            .entity(e)
            .insert((ProjectedAnchor::Dynamic, ProjectionState::default()));
    }
}

/// `V` keybind toggles `MapViewMode`. Egui-focus-gated like the speed
/// keybinds (`1/2/3`, `Space`). Camera recentering is handled separately
/// by `camera_recenter_on_mode_change_system` so the keypress path and
/// HUD-button path go through the same adjustment.
pub fn toggle_view_mode_system(
    mut contexts: EguiContexts,
    keys: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<MapViewMode>,
) {
    let typing = contexts.ctx_mut().wants_keyboard_input();
    if typing {
        return;
    }
    if keys.just_pressed(KeyCode::KeyV) {
        *mode = mode.toggled();
        info!("MapViewMode toggled to {:?}", *mode);
    }
}

/// Detect HUD-button toggles (`MapViewMode` mutated outside the keypress
/// path) and rotate the camera so the same logical tile stays centred.
/// Without this the camera's view-space position points to different
/// logical tiles before vs. after the toggle, which leaves the visible
/// area empty until streaming catches up — the `V`-toggle-to-black-screen
/// regression. Tracks the last-seen mode in a `Local`.
pub fn camera_recenter_on_mode_change_system(
    mode: Res<MapViewMode>,
    proj: Res<MapProjection>,
    mut last_mode: Local<Option<MapViewMode>>,
    mut camera_q: Query<&mut Transform, With<Camera>>,
) {
    let cur = *mode;
    let prev = match *last_mode {
        Some(m) => m,
        None => {
            *last_mode = Some(cur);
            return;
        }
    };
    if prev != cur {
        adjust_camera_for_mode_change(prev, cur, &proj, &mut camera_q);
        *last_mode = Some(cur);
    }
}

/// Re-anchor the camera so the logical tile previously under its centre
/// stays under its centre after a mode change. In TopDown the camera y
/// equals the logical y; in Tilted, camera y = logical y * y_scale.
fn adjust_camera_for_mode_change(
    from: MapViewMode,
    to: MapViewMode,
    proj: &MapProjection,
    camera_q: &mut Query<&mut Transform, With<Camera>>,
) {
    if from == to {
        return;
    }
    let Ok(mut tf) = camera_q.get_single_mut() else {
        return;
    };
    let view_xy = tf.translation.truncate();
    // Recover the logical position of the camera under the previous mode,
    // then re-project to the new mode.
    let logical = camera_view_to_logical(view_xy, from, proj);
    let new_view = logical_to_view_camera(logical, to, proj);
    tf.translation.x = new_view.x;
    tf.translation.y = new_view.y;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topdown_is_xy_identity_with_ysort_dz() {
        let proj = MapProjection::default();
        let v = Vec3::new(123.0, 456.0, 0.5);
        let p = project(v, 7, MapViewMode::TopDown, &proj);
        // X and Y are unchanged in TopDown — only Z carries the tiny
        // per-y depth offset that lets bottom-anchored sprites y-sort.
        assert_eq!((p.x, p.y), (v.x, v.y));
        assert!(p.z < v.z, "TopDown should push z slightly negative for y-sort");
        let (dy, dz) = project_delta(456.0, 7, MapViewMode::TopDown, &proj);
        assert_eq!(dy, 0.0);
        // dz = -y * 0.0001 - elev * 0.0005 = -0.0456 - 0.0035 = -0.0491
        assert!((dz - (-0.0491)).abs() < 1e-4, "got {dz}");
    }

    #[test]
    fn tilted_compresses_y_and_lifts_by_elev() {
        let proj = MapProjection::default();
        let v = Vec3::new(100.0, 1000.0, 0.5);
        let p = project(v, 4, MapViewMode::Tilted, &proj);
        // X preserved.
        assert!((p.x - 100.0).abs() < 1e-3);
        // Y: 1000 * 0.58 + 4*6 = 580 + 24 = 604.
        assert!((p.y - 604.0).abs() < 1e-3);
    }

    #[test]
    fn tilted_unproject_is_inverse_at_plane_z() {
        let proj = MapProjection::default();
        // Pick a logical position at z=0; project then unproject; recover.
        let logical = Vec2::new(800.0, 1234.5);
        let view_y = logical.y * proj.y_scale + 0.0 * proj.z_step_px;
        let view = Vec2::new(logical.x, view_y);
        let recovered = unproject_to_world(view, 0, MapViewMode::Tilted, &proj);
        assert!((recovered.x - logical.x).abs() < 1e-3);
        assert!((recovered.y - logical.y).abs() < 1e-3);
    }

    #[test]
    fn tilted_dz_below_day_night() {
        // Regression: any projected anchor's depth-z offset (even at the
        // worst-case tile_y / elev) must stay well below the day-night
        // overlay's z=90 so the overlay still tints everything beneath it.
        // Sample a wide grid of (tile_y, elev) pairs using the world's
        // configured Z range and the maximum y_scale we'd ever pick.
        let proj = MapProjection::default();
        let day_night_z = crate::rendering::day_night::OVERLAY_Z;
        let kind_bias_max: f32 = 1.0; // entity z constants top out near 0.5
                                      // Sweep a generous tile_y range and full elev range.
        for tile_y_steps in 0..200 {
            let logical_y = tile_y_steps as f32 * 1024.0; // up to ~204800 tile-units
            for &elev_z in &[
                crate::world::chunk::Z_MIN as i8,
                0,
                crate::world::chunk::Z_MAX as i8,
            ] {
                let (_, dz) = project_delta(logical_y, elev_z, MapViewMode::Tilted, &proj);
                let final_z = kind_bias_max + dz;
                assert!(
                    final_z < day_night_z - 1.0,
                    "projected z {final_z} too close to day-night z {day_night_z} (logical_y {logical_y}, elev_z {elev_z})"
                );
            }
        }
    }

    #[test]
    fn camera_helpers_are_inverse() {
        let proj = MapProjection::default();
        let logical = Vec2::new(500.0, 800.0);
        let camera = logical_to_view_camera(logical, MapViewMode::Tilted, &proj);
        let recovered = camera_view_to_logical(camera, MapViewMode::Tilted, &proj);
        assert!((recovered.x - logical.x).abs() < 1e-3);
        assert!((recovered.y - logical.y).abs() < 1e-3);
    }
}
