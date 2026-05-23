//! Debug Test-Drive manual input — WASD/QE tap-to-step controls for a
//! Vehicle armed via the designer's "Test Drive" button.
//!
//! Coexists with the existing right-click `VehicleOrderKind::MoveTo` flow:
//! while a `VehiclePathFollow` is in flight (from either source) movement
//! keys are dropped (no buffering); press `S` to cancel the current step,
//! then press `W` to take over.
//!
//! `Esc` clears `ManualDriveState.active`, releasing the camera-pan
//! suppression in `camera::camera_input_system`. Egui keyboard focus is
//! gated upstream — typing in the designer's name field doesn't drive.
//!
//! See plan: `~/.claude/plans/evaluate-the-users-xiao1-civgame-plans-v-optimized-squirrel.md`.

use bevy::prelude::*;
use bevy_egui::EguiContexts;

use crate::simulation::vehicle::{
    plan_manual_step, ManualDriveState, ManualIntent, Vehicle, VehicleDesignRegistry,
    VehicleOccupancyIndex, VehiclePathFollow,
};
use crate::world::chunk::ChunkMap;

/// Update-set system. Reads `just_pressed` so a held W taps once per frame
/// rather than spamming dispatches; the next press only registers once the
/// current `VehiclePathFollow` has completed (per the design's tap-to-step
/// choice). `S` clears an in-flight `VehiclePathFollow`. `Esc` releases
/// manual drive entirely. Egui keyboard focus suppresses every key so the
/// designer's text fields keep working.
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
    let intent = if keys.just_pressed(KeyCode::KeyW) || keys.just_pressed(KeyCode::ArrowUp) {
        Some(ManualIntent::Forward)
    } else if keys.just_pressed(KeyCode::KeyA) || keys.just_pressed(KeyCode::ArrowLeft) {
        Some(ManualIntent::TurnCCW)
    } else if keys.just_pressed(KeyCode::KeyD) || keys.just_pressed(KeyCode::ArrowRight) {
        Some(ManualIntent::TurnCW)
    } else if keys.just_pressed(KeyCode::KeyQ) {
        Some(ManualIntent::ForwardLeft)
    } else if keys.just_pressed(KeyCode::KeyE) {
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
