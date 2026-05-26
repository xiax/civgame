//! Debug Test-Drive manual input — WASD/QE hold-to-step controls for a
//! Vehicle armed via the designer's "Test Drive" button.
//!
//! Movement keys (W/A/D/Q/E) read `pressed`: as soon as the current
//! `VehiclePathFollow` clears the next held intent dispatches a fresh
//! step. `S` and `Esc` stay `just_pressed` so cancel + release are
//! deliberate one-shot actions. While a `VehiclePathFollow` is in flight
//! (from either source) movement keys are dropped — no buffering.
//!
//! `Esc` clears `ManualDriveState.active`, releasing the camera-pan
//! suppression in `camera::camera_input_system`. Egui keyboard focus is
//! gated upstream — typing in the designer's name field doesn't drive.

use bevy::prelude::*;
use bevy_egui::EguiContexts;

use crate::simulation::vehicle::{
    plan_manual_step, ManualDriveState, ManualIntent, PlayerPiloted, Vehicle,
    VehicleDesignRegistry, VehicleOccupancyIndex, VehiclePathFollow,
};
use crate::world::chunk::ChunkMap;

/// Update-set system. Movement keys read `pressed` so holding W keeps
/// committing steps as each `VehiclePathFollow` clears; the in-flight gate
/// prevents queueing a step while the prior one is still walking. `S`
/// (clear in-flight path) and `Esc` (release manual drive) stay
/// `just_pressed` — both are deliberate one-shot actions. Egui keyboard
/// focus suppresses every key so the designer's text fields keep working.
pub fn manual_drive_input_system(
    mut commands: Commands,
    mut contexts: EguiContexts,
    keys: Res<ButtonInput<KeyCode>>,
    mut manual: ResMut<ManualDriveState>,
    registry: Res<VehicleDesignRegistry>,
    occupancy: Res<VehicleOccupancyIndex>,
    chunk_map: Res<ChunkMap>,
    vehicles: Query<(&Vehicle, Option<&VehiclePathFollow>)>,
) {
    let Some(active) = manual.active else {
        return;
    };
    // Self-clear if the vehicle is gone.
    let Ok((vehicle, follow)) = vehicles.get(active) else {
        manual.active = None;
        manual.last_status = Some("Manual drive: vehicle gone".to_string());
        return;
    };
    if contexts.ctx_mut().wants_keyboard_input() {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        commands.entity(active).remove::<PlayerPiloted>();
        manual.active = None;
        manual.last_status = Some("Manual drive: released".to_string());
        return;
    }
    if keys.just_pressed(KeyCode::KeyS) || keys.just_pressed(KeyCode::ArrowDown) {
        if follow.is_some() {
            commands.entity(active).remove::<VehiclePathFollow>();
            manual.last_status = Some("Manual drive: stopped".to_string());
        }
        return;
    }
    // Movement keys are ignored while a step (manual or right-click MoveTo)
    // is still being walked — tap-to-step, no buffering.
    if follow.is_some() {
        return;
    }
    // Hold-to-drive: movement keys are `pressed` so the next held intent
    // dispatches the moment the prior `VehiclePathFollow` clears (above).
    let intent = if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
        Some(ManualIntent::Forward)
    } else if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        Some(ManualIntent::TurnCCW)
    } else if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        Some(ManualIntent::TurnCW)
    } else if keys.pressed(KeyCode::KeyQ) {
        Some(ManualIntent::ForwardLeft)
    } else if keys.pressed(KeyCode::KeyE) {
        Some(ManualIntent::ForwardRight)
    } else {
        None
    };
    let Some(intent) = intent else {
        return;
    };
    let Some(design) = registry.get(vehicle.design_id) else {
        return;
    };
    match plan_manual_step(vehicle, design, intent, &chunk_map, &occupancy, active) {
        Some(path) => {
            commands.entity(active).insert(VehiclePathFollow {
                path,
                cursor: 1,
                tip_torque: 0.0,
            });
            manual.last_status = Some(format!("Manual drive: {:?}", intent));
        }
        None => {
            manual.last_status = Some(format!("Manual drive: blocked ({:?})", intent));
        }
    }
}
