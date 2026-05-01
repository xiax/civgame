use crate::world::chunk::CHUNK_SIZE;
use crate::world::globe::{GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::TILE_SIZE;
use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::{MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy_egui::EguiContexts;

const PAN_SPEED: f32 = 400.0;
const ZOOM_SPEED: f32 = 0.15;
const PINCH_ZOOM_SPEED: f32 = 2.0;
const MIN_SCALE: f32 = 0.25;
const MAX_SCALE: f32 = 8.0;

#[derive(Resource)]
pub struct CameraState {
    pub zoom: f32,
    pub drag_origin: Option<Vec2>,
}

/// Current Z-level the player is viewing. i32::MAX = surface mode (default).
/// PageDown decreases (reveals underground), PageUp increases (toward surface).
#[derive(Resource)]
pub struct CameraViewZ(pub i32);

impl Default for CameraViewZ {
    fn default() -> Self {
        CameraViewZ(i32::MAX)
    }
}

impl Default for CameraState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            drag_origin: None,
        }
    }
}

pub fn setup_camera(mut commands: Commands) {
    // Start at the center of the globe (globe cell 32,16 = chunk 512,256 = pixel 8192,4096)
    let globe_cx = (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS;
    let globe_cy = (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS;
    let start_x = globe_cx as f32 * CHUNK_SIZE as f32 * TILE_SIZE;
    let start_y = globe_cy as f32 * CHUNK_SIZE as f32 * TILE_SIZE;

    commands.spawn((
        Camera2d,
        Transform::from_xyz(start_x, start_y, 100.0),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));
}

pub fn camera_input_system(
    mut contexts: EguiContexts,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mut scroll_events: EventReader<MouseWheel>,
    mut motion_events: EventReader<MouseMotion>,
    mut pinch_events: EventReader<PinchGesture>,
    mut camera_state: ResMut<CameraState>,
    mut camera_view_z: ResMut<CameraViewZ>,
    mut camera_query: Query<(&mut Transform, &mut OrthographicProjection), With<Camera>>,
) {
    let Ok((mut transform, mut projection)) = camera_query.get_single_mut() else {
        return;
    };

    let egui_wants_mouse =
        contexts.ctx_mut().wants_pointer_input() || contexts.ctx_mut().is_pointer_over_area();

    let dt = time.delta_secs();
    let speed = PAN_SPEED * projection.scale;

    // WASD panning
    let mut pan = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
        pan.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
        pan.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        pan.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        pan.x += 1.0;
    }

    if pan != Vec2::ZERO {
        let delta = pan.normalize() * speed * dt;
        transform.translation.x += delta.x;
        transform.translation.y += delta.y;
    }

    // Middle-mouse drag
    if mouse_buttons.pressed(MouseButton::Middle) && !egui_wants_mouse {
        for ev in motion_events.read() {
            transform.translation.x -= ev.delta.x * projection.scale;
            transform.translation.y += ev.delta.y * projection.scale;
        }
    } else {
        motion_events.clear();
    }

    // Scroll: trackpad two-finger swipe pans, mouse wheel zooms
    for ev in scroll_events.read() {
        if egui_wants_mouse {
            continue;
        }
        match ev.unit {
            MouseScrollUnit::Pixel => {
                transform.translation.x -= ev.x * projection.scale;
                transform.translation.y += ev.y * projection.scale;
            }
            MouseScrollUnit::Line => {
                projection.scale =
                    (projection.scale * (1.0 - ev.y * ZOOM_SPEED)).clamp(MIN_SCALE, MAX_SCALE);
                camera_state.zoom = projection.scale;
            }
        }
    }

    // Pinch zoom (trackpad)
    for ev in pinch_events.read() {
        if egui_wants_mouse {
            continue;
        }
        projection.scale =
            (projection.scale * (1.0 - ev.0 * PINCH_ZOOM_SPEED)).clamp(MIN_SCALE, MAX_SCALE);
        camera_state.zoom = projection.scale;
    }

    // Z-level viewer: PageDown reveals deeper layers, PageUp returns toward surface
    let z_min = crate::world::chunk::Z_MIN;
    let z_max = crate::world::chunk::Z_MAX;
    if keys.just_pressed(KeyCode::PageDown) {
        let cur = if camera_view_z.0 == i32::MAX {
            z_max
        } else {
            camera_view_z.0
        };
        camera_view_z.0 = (cur - 1).max(z_min);
    }
    if keys.just_pressed(KeyCode::PageUp) {
        if camera_view_z.0 < i32::MAX {
            let next = camera_view_z.0 + 1;
            camera_view_z.0 = if next > z_max { i32::MAX } else { next };
        }
    }

    // Clamp camera to globe bounds
    let globe_w = (GLOBE_WIDTH * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32 * TILE_SIZE;
    let globe_h = (GLOBE_HEIGHT * GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32) as f32 * TILE_SIZE;
    transform.translation.x = transform.translation.x.clamp(0.0, globe_w);
    transform.translation.y = transform.translation.y.clamp(0.0, globe_h);
}
