use crate::simulation::person::Person;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

#[derive(Resource, Default)]
pub struct SelectedEntity(pub Option<Entity>);

pub fn click_to_select_system(
    mut contexts: EguiContexts,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_query: Query<(&Camera, &GlobalTransform), With<Camera>>,
    persons: Query<(Entity, &Transform), With<Person>>,
    mut selected: ResMut<SelectedEntity>,
) {
    if contexts.ctx_mut().is_pointer_over_area() || contexts.ctx_mut().wants_pointer_input() {
        return;
    }

    if !mouse_buttons.just_pressed(MouseButton::Left) {
        return;
    }

    let Ok(window) = windows.get_single() else {
        return;
    };
    let Ok((camera, cam_transform)) = camera_query.get_single() else {
        return;
    };
    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };

    let Ok(world_pos) = camera.viewport_to_world_2d(cam_transform, cursor_pos) else {
        return;
    };

    let mut best: Option<(Entity, f32)> = None;
    for (entity, transform) in persons.iter() {
        let pos = transform.translation.truncate();
        let dist = pos.distance(world_pos);
        if dist < 12.0 {
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((entity, dist));
            }
        }
    }

    selected.0 = best.map(|(e, _)| e);
}
