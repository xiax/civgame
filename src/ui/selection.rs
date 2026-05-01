use crate::simulation::faction::{FactionMember, PlayerFaction};
use crate::simulation::person::{Drafted, Person};
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

/// The currently focused entity. Single-entity panels (inspector, hover,
/// path-debug gizmos, work-order context menu) read this.
#[derive(Resource, Default)]
pub struct SelectedEntity(pub Option<Entity>);

/// All entities currently selected. Always includes `SelectedEntity.0` if
/// non-empty. A drag-rect populates it; a single click resets it to one entry.
#[derive(Resource, Default)]
pub struct SelectedEntities {
    pub ids: Vec<Entity>,
}

/// In-progress selection drag. `start_world` is set on left-mouse-press,
/// `current_world` is updated each frame while held; both are cleared on
/// release. While `start_world.is_some()`, the gizmo system draws a rect.
#[derive(Resource, Default)]
pub struct SelectionDrag {
    pub start_world: Option<Vec2>,
    pub current_world: Option<Vec2>,
}

/// Pixels (world units) below which a press→release is treated as a single
/// click rather than a drag. Tiles are 16 px; 6 keeps clicks tolerant of
/// mouse jitter without eating intentional drags.
const DRAG_THRESHOLD_PX: f32 = 6.0;
const SINGLE_CLICK_RADIUS_PX: f32 = 12.0;

pub fn selection_input_system(
    mut contexts: EguiContexts,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera>>,
    persons: Query<(Entity, &Transform, Option<&FactionMember>), With<Person>>,
    player_faction: Res<PlayerFaction>,
    mut selected: ResMut<SelectedEntity>,
    mut selected_many: ResMut<SelectedEntities>,
    mut drag: ResMut<SelectionDrag>,
) {
    if contexts.ctx_mut().is_pointer_over_area() || contexts.ctx_mut().wants_pointer_input() {
        // Clicking on a panel should never start a drag, but if a drag is
        // already in flight, let it complete normally below.
        if drag.start_world.is_none() {
            return;
        }
    }

    let Ok(window) = windows.get_single() else {
        return;
    };
    let Ok((camera, cam_transform)) = camera_query.get_single() else {
        return;
    };
    let cursor_world: Option<Vec2> = window
        .cursor_position()
        .and_then(|cp| camera.viewport_to_world_2d(cam_transform, cp).ok());

    if mouse_buttons.just_pressed(MouseButton::Left) {
        if let Some(p) = cursor_world {
            drag.start_world = Some(p);
            drag.current_world = Some(p);
        }
    } else if mouse_buttons.pressed(MouseButton::Left) {
        if let Some(p) = cursor_world {
            drag.current_world = Some(p);
        }
    }

    if !mouse_buttons.just_released(MouseButton::Left) {
        return;
    }

    let start = drag.start_world.take();
    let end = drag.current_world.take().or(cursor_world);
    let (Some(start), Some(end)) = (start, end) else {
        return;
    };

    if start.distance(end) < DRAG_THRESHOLD_PX {
        // Single-click: pick nearest Person within radius.
        let mut best: Option<(Entity, f32)> = None;
        for (entity, transform, _faction) in persons.iter() {
            let pos = transform.translation.truncate();
            let dist = pos.distance(end);
            if dist < SINGLE_CLICK_RADIUS_PX && best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((entity, dist));
            }
        }
        match best {
            Some((e, _)) => {
                selected.0 = Some(e);
                selected_many.ids = vec![e];
            }
            None => {
                selected.0 = None;
                selected_many.ids.clear();
            }
        }
        return;
    }

    // Drag-select: collect all player-faction Persons inside the rect.
    let min = Vec2::new(start.x.min(end.x), start.y.min(end.y));
    let max = Vec2::new(start.x.max(end.x), start.y.max(end.y));
    let mut hits: Vec<Entity> = Vec::new();
    for (entity, transform, faction) in persons.iter() {
        let Some(member) = faction else { continue };
        if member.faction_id != player_faction.faction_id {
            continue;
        }
        let p = transform.translation.truncate();
        if p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y {
            hits.push(entity);
        }
    }
    selected.0 = hits.first().copied();
    selected_many.ids = hits;
}

/// Draws the in-progress drag rectangle and a colored ring under each
/// selected entity (red for drafted, yellow otherwise).
pub fn selection_gizmo_system(
    drag: Res<SelectionDrag>,
    selected_many: Res<SelectedEntities>,
    transforms: Query<&Transform>,
    drafted_q: Query<(), With<Drafted>>,
    mut gizmos: Gizmos,
) {
    if let (Some(s), Some(c)) = (drag.start_world, drag.current_world) {
        let center = (s + c) * 0.5;
        let size = (c - s).abs();
        if size.length_squared() > 0.0 {
            gizmos.rect_2d(center, size, Color::srgba(0.4, 1.0, 0.4, 0.85));
        }
    }

    for &e in &selected_many.ids {
        let Ok(t) = transforms.get(e) else { continue };
        let pos = t.translation.truncate();
        let color = if drafted_q.get(e).is_ok() {
            Color::srgba(1.0, 0.35, 0.25, 0.95)
        } else {
            Color::srgba(1.0, 0.95, 0.25, 0.85)
        };
        gizmos.circle_2d(pos, 8.0, color);
    }
}
