use super::selection::SelectedEntity;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::rendering::camera::CameraViewZ;
use crate::simulation::construction::{
    faction_can_build, recipe_for, BedMap, Blueprint, BlueprintMap, BuildSiteKind, CampfireMap,
    ChairMap, DoorMap, LoomMap, TableMap, WallMaterial, WorkbenchMap,
};
use crate::simulation::faction::SOLO;
use crate::simulation::faction::{FactionMember, FactionRegistry, FactionTechs, PlayerFaction};
use crate::simulation::items::GroundItem;
use crate::simulation::person::{AiState, PersonAI, PlayerOrder, PlayerOrderKind};
use crate::simulation::plants::PlantMap;
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
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
    /// Top-level actions shown directly (Move, Mine, Gather, …).
    pub actions: Vec<PlayerOrderKind>,
    /// Build options nested under the "Build ▸" submenu. `bool` is whether the
    /// player faction has the required tech — locked options render greyed-out.
    pub build_options: Vec<(PlayerOrderKind, bool)>,
}

/// All build options the player could potentially place on an open tile.
fn all_build_options() -> [BuildSiteKind; 12] {
    [
        BuildSiteKind::Wall(WallMaterial::Palisade),
        BuildSiteKind::Wall(WallMaterial::WattleDaub),
        BuildSiteKind::Wall(WallMaterial::Stone),
        BuildSiteKind::Wall(WallMaterial::Mudbrick),
        BuildSiteKind::Wall(WallMaterial::CutStone),
        BuildSiteKind::Door,
        BuildSiteKind::Bed,
        BuildSiteKind::Campfire,
        BuildSiteKind::Workbench,
        BuildSiteKind::Loom,
        BuildSiteKind::Table,
        BuildSiteKind::Chair,
    ]
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
    routing: (
        Res<ChunkGraph>,
        Res<ChunkRouter>,
        Res<ChunkConnectivity>,
        Res<CameraViewZ>,
        Res<BedMap>,
        Res<CampfireMap>,
        Res<DoorMap>,
        Res<TableMap>,
        Res<ChairMap>,
        Res<WorkbenchMap>,
        Res<LoomMap>,
        ResMut<BlueprintMap>,
    ),
    mut menu_state: ResMut<ContextMenuState>,
    mut commands: Commands,
) {
    let (
        chunk_graph,
        chunk_router,
        chunk_connectivity,
        camera_view_z,
        bed_map,
        campfire_map,
        door_map,
        table_map,
        chair_map,
        workbench_map,
        loom_map,
        mut bp_order_map,
    ) = routing;

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
                    let mut build_options: Vec<(PlayerOrderKind, bool)> = Vec::new();

                    let player_techs: FactionTechs = faction_q
                        .get(sel_entity)
                        .ok()
                        .and_then(|m| faction_registry.factions.get(&m.faction_id))
                        .map(|f| f.techs.clone())
                        .unwrap_or_default();

                    let pos_tile = (tx as i16, ty as i16);
                    let already_built = bed_map.0.contains_key(&pos_tile)
                        || campfire_map.0.contains_key(&pos_tile)
                        || door_map.0.contains_key(&pos_tile)
                        || table_map.0.contains_key(&pos_tile)
                        || chair_map.0.contains_key(&pos_tile)
                        || workbench_map.0.contains_key(&pos_tile)
                        || loom_map.0.contains_key(&pos_tile);

                    if let Some(kind) = target_kind {
                        if matches!(kind, TileKind::Wall | TileKind::Stone) {
                            actions.push(PlayerOrderKind::Mine);
                        }
                        if kind.is_passable() && !underground {
                            actions.push(PlayerOrderKind::DigDown);
                            // Build menu: list every option, gating advanced
                            // ones by tech. Skip if a structure is already on
                            // this tile.
                            if !already_built {
                                for bk in all_build_options() {
                                    let unlocked = faction_can_build(bk, &player_techs);
                                    build_options.push((PlayerOrderKind::Build(bk), unlocked));
                                }
                            }
                        }
                    }
                    if !underground && already_built {
                        actions.push(PlayerOrderKind::Deconstruct);
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
                    menu_state.target_tile = pos_tile;
                    menu_state.target_z = target_z_i32 as i8;
                    menu_state.actions = actions;
                    menu_state.build_options = build_options;
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
    let build_options = menu_state.build_options.clone();
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
                if !build_options.is_empty() {
                    ui.menu_button("Build \u{25B8}", |ui| {
                        for (opt, unlocked) in &build_options {
                            let label = match opt {
                                PlayerOrderKind::Build(bk) => recipe_for(*bk).name,
                                _ => opt.label(),
                            };
                            let btn = egui::Button::new(label);
                            let resp = ui.add_enabled(*unlocked, btn);
                            if resp.clicked() {
                                chosen = Some(*opt);
                                ui.close_menu();
                            }
                        }
                    });
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

            // For Build orders: spawn a personal Blueprint at the target tile
            // so the agent has a concrete entity to work toward.
            let build_bp: Option<Entity> = if let PlayerOrderKind::Build(kind) = action {
                if !bp_order_map.0.contains_key(&target_tile) {
                    let faction_id = faction_q
                        .get(sel_entity)
                        .map(|m| m.faction_id)
                        .unwrap_or(SOLO);
                    let wp = tile_to_world(target_tile.0 as i32, target_tile.1 as i32);
                    let target_z = chunk_map
                        .surface_z_at(target_tile.0 as i32, target_tile.1 as i32)
                        as i8;
                    let bp_e = commands
                        .spawn((
                            Blueprint::new(faction_id, Some(sel_entity), kind, target_tile, target_z),
                            Transform::from_xyz(wp.x, wp.y, 0.3),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    bp_order_map.0.insert(target_tile, bp_e);
                    Some(bp_e)
                } else {
                    bp_order_map.0.get(&target_tile).copied()
                }
            } else {
                None
            };

            let task = match action {
                PlayerOrderKind::Move => TaskKind::Idle,
                PlayerOrderKind::Mine => TaskKind::Gather,
                PlayerOrderKind::Gather => TaskKind::Gather,
                PlayerOrderKind::PickUp => TaskKind::Scavenge,
                PlayerOrderKind::Build(BuildSiteKind::Bed) => TaskKind::ConstructBed,
                PlayerOrderKind::Build(_) => TaskKind::Construct,
                PlayerOrderKind::DigDown => TaskKind::Dig,
                PlayerOrderKind::Deconstruct => TaskKind::Deconstruct,
            };
            assign_task_with_routing(
                &mut ai,
                (cur_tx as i16, cur_ty as i16),
                cur_chunk,
                target_tile,
                task,
                build_bp,
                &chunk_graph,
                &chunk_router,
                &chunk_map,
                &chunk_connectivity,
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
