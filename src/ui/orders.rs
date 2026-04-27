use super::selection::SelectedEntity;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::rendering::camera::CameraViewZ;
use crate::simulation::faction::{FactionMember, FactionRegistry, PlayerFaction};
use crate::simulation::items::GroundItem;
use crate::simulation::person::{AiState, PersonAI, PlayerOrder, PlayerOrderKind};
use crate::simulation::plants::PlantMap;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::PERM_SETTLEMENT;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{egui, EguiContexts};

#[derive(Resource, Default)]
pub struct ContextMenuState {
    pub open: bool,
    pub screen_pos: egui::Pos2,
    pub target_tile: (i16, i16),
    /// Foot Z of the targeted tile at the moment of right-click.
    pub target_z: i8,
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
    faction_registry: Res<FactionRegistry>,
    mut ai_q: Query<(&mut PersonAI, &Transform)>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    spatial: Res<SpatialIndex>,
    ground_item_check: Query<(), With<GroundItem>>,
    routing: (Res<ChunkGraph>, Res<CameraViewZ>),
    mut menu_state: ResMut<ContextMenuState>,
    mut commands: Commands,
) {
    let (chunk_graph, camera_view_z) = routing;
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

                    // Determine the Z slice this click targets. Surface
                    // mode (CameraViewZ::MAX) → click targets the tile's
                    // surface_z. Underground view → click targets the
                    // camera_view_z slice and the tile read uses tile_at.
                    let underground = camera_view_z.0 != i32::MAX;
                    let target_z_i32 = if underground {
                        camera_view_z.0
                    } else {
                        chunk_map.surface_z_at(tx, ty)
                    };
                    let target_kind = if underground {
                        Some(chunk_map.tile_at(tx, ty, target_z_i32).kind)
                    } else {
                        chunk_map.tile_kind_at(tx, ty)
                    };

                    let mut actions = vec![PlayerOrderKind::Move];
                    if let Some(kind) = target_kind {
                        if matches!(kind, TileKind::Wall | TileKind::Stone) {
                            actions.push(PlayerOrderKind::Mine);
                        }
                        if kind.is_passable() && !underground {
                            actions.push(PlayerOrderKind::DigDown);
                            let has_perm = faction_q
                                .get(sel_entity)
                                .ok()
                                .and_then(|m| faction_registry.factions.get(&m.faction_id))
                                .map(|f| f.techs.has(PERM_SETTLEMENT))
                                .unwrap_or(false);

                            if has_perm {
                                actions.push(PlayerOrderKind::BuildWall);
                            }
                            actions.push(PlayerOrderKind::BuildBed);
                        }
                    }
                    if !underground && plant_map.0.contains_key(&(tx, ty)) {
                        actions.push(PlayerOrderKind::Gather);
                    }
                    if !underground {
                        for &e in spatial.get(tx, ty) {
                            if ground_item_check.get(e).is_ok() {
                                actions.push(PlayerOrderKind::PickUp);
                                break;
                            }
                        }
                    }

                    menu_state.open = true;
                    menu_state.screen_pos = egui::pos2(cursor_pos.x, cursor_pos.y);
                    menu_state.target_tile = (tx as i16, ty as i16);
                    menu_state.target_z = target_z_i32 as i8;
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
    let target_z = menu_state.target_z;
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
            let task = match action {
                PlayerOrderKind::Move => TaskKind::Idle,
                PlayerOrderKind::Mine => TaskKind::Gather,
                PlayerOrderKind::Gather => TaskKind::Gather,
                PlayerOrderKind::PickUp => TaskKind::Scavenge,
                PlayerOrderKind::BuildWall => TaskKind::Construct,
                PlayerOrderKind::BuildBed => TaskKind::ConstructBed,
                PlayerOrderKind::DigDown => TaskKind::Dig,
            };
            assign_task_with_routing(
                &mut ai,
                cur_chunk,
                target_tile,
                task,
                None,
                &chunk_graph,
                &chunk_map,
            );
            ai.target_z = target_z;
        }
        commands.entity(sel_entity).insert(PlayerOrder {
            order: action,
            target_tile,
            target_z,
        });
        menu_state.open = false;
    }
}

pub fn player_order_completion_system(
    mut commands: Commands,
    query: Query<(Entity, &PersonAI), With<PlayerOrder>>,
) {
    for (entity, ai) in query.iter() {
        if ai.state == AiState::Idle && ai.task_id == PersonAI::UNEMPLOYED {
            commands.entity(entity).remove::<PlayerOrder>();
        }
    }
}
