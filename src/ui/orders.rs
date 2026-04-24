use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{egui, EguiContexts};
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use crate::world::spatial::SpatialIndex;
use crate::simulation::faction::{FactionMember, PlayerFaction};
use crate::simulation::items::GroundItem;
use crate::simulation::jobs::{JobKind, assign_job_with_routing};
use crate::simulation::person::{AiState, PersonAI, PlayerOrder, PlayerOrderKind};
use crate::simulation::plants::PlantMap;
use super::selection::SelectedEntity;

#[derive(Resource, Default)]
pub struct ContextMenuState {
    pub open: bool,
    pub screen_pos: egui::Pos2,
    pub target_tile: (i16, i16),
    pub actions: Vec<PlayerOrderKind>,
}

pub fn right_click_context_menu_system(
    mut contexts: EguiContexts,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<Camera2d>>,
    selected: Res<SelectedEntity>,
    player_faction: Res<PlayerFaction>,
    faction_q: Query<&FactionMember>,
    mut ai_q: Query<(&mut PersonAI, &Transform)>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    spatial: Res<SpatialIndex>,
    ground_item_check: Query<(), With<GroundItem>>,
    chunk_graph: Res<ChunkGraph>,
    mut menu_state: ResMut<ContextMenuState>,
    mut commands: Commands,
) {
    // Require a selected player-faction member.
    let Some(sel_entity) = selected.0 else {
        menu_state.open = false;
        return;
    };
    let is_player_member = faction_q
        .get(sel_entity)
        .map(|m| m.faction_id == player_faction.faction_id)
        .unwrap_or(false);
    if !is_player_member {
        menu_state.open = false;
        return;
    }

    let ctx = contexts.ctx_mut();

    // Detect right-click in the world (not over any egui panel).
    if !ctx.is_pointer_over_area() && mouse_buttons.just_pressed(MouseButton::Right) {
        if let (Ok(window), Ok((camera, cam_transform))) =
            (windows.get_single(), camera_q.get_single())
        {
            if let Some(cursor_pos) = window.cursor_position() {
                if let Ok(world_pos) = camera.viewport_to_world_2d(cam_transform, cursor_pos) {
                    let tx = (world_pos.x / TILE_SIZE).floor() as i32;
                    let ty = (world_pos.y / TILE_SIZE).floor() as i32;

                    let mut actions = vec![PlayerOrderKind::Move];
                    if let Some(kind) = chunk_map.tile_kind_at(tx, ty) {
                        if matches!(kind, TileKind::Stone | TileKind::Wall) {
                            actions.push(PlayerOrderKind::Mine);
                        }
                    }
                    if plant_map.0.contains_key(&(tx, ty)) {
                        actions.push(PlayerOrderKind::Gather);
                    }
                    for &e in spatial.get(tx, ty) {
                        if ground_item_check.get(e).is_ok() {
                            actions.push(PlayerOrderKind::PickUp);
                            break;
                        }
                    }

                    menu_state.open = true;
                    menu_state.screen_pos = egui::pos2(cursor_pos.x, cursor_pos.y);
                    menu_state.target_tile = (tx as i16, ty as i16);
                    menu_state.actions = actions;
                }
            }
        }
    }

    // Close on left-click outside the menu.
    if menu_state.open
        && !ctx.is_pointer_over_area()
        && mouse_buttons.just_pressed(MouseButton::Left)
    {
        menu_state.open = false;
    }

    if !menu_state.open {
        return;
    }

    let actions = menu_state.actions.clone();
    let target_tile = menu_state.target_tile;
    let mut chosen: Option<PlayerOrderKind> = None;

    egui::Area::new("context_menu".into())
        .fixed_pos(menu_state.screen_pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                for action in &actions {
                    if ui.button(action.label()).clicked() {
                        chosen = Some(*action);
                    }
                }
            });
        });

    if let Some(action) = chosen {
        if let Ok((mut ai, transform)) = ai_q.get_mut(sel_entity) {
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );
            let job = match action {
                PlayerOrderKind::Move => JobKind::Idle,
                _ => JobKind::Gather,
            };
            assign_job_with_routing(&mut ai, cur_chunk, target_tile, job, &chunk_graph, &chunk_map);
        }
        commands.entity(sel_entity).insert(PlayerOrder { order: action, target_tile });
        menu_state.open = false;
    }
}

pub fn player_order_completion_system(
    mut commands: Commands,
    query: Query<(Entity, &PersonAI), With<PlayerOrder>>,
) {
    for (entity, ai) in query.iter() {
        if ai.state == AiState::Idle && ai.job_id == PersonAI::UNEMPLOYED {
            commands.entity(entity).remove::<PlayerOrder>();
        }
    }
}
