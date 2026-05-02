use super::selection::{SelectedEntities, SelectedEntity};
use crate::economy::item::Item;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::rendering::camera::CameraViewZ;
use crate::simulation::animals::{Deer, Fox, Wolf};
use crate::simulation::combat::{CombatTarget, Health};
use crate::simulation::construction::{
    faction_can_build, recipe_for, BedMap, Blueprint, BlueprintMap, BuildSiteKind, CampfireMap,
    ChairMap, DoorMap, LoomMap, TableMap, WallMaterial, WorkbenchMap,
};
use crate::simulation::corpse::Corpse;
use crate::simulation::faction::SOLO;
use crate::simulation::faction::{FactionMember, FactionRegistry, FactionTechs, PlayerFaction};
use crate::simulation::items::{GroundItem, TargetItem};
use crate::simulation::person::{
    AiState, Drafted, Person, PersonAI, PlayerOrder, PlayerOrderKind, Profession,
};
use crate::simulation::plants::PlantMap;
use crate::simulation::tasks::{assign_task_with_routing, task_interacts_from_adjacent, TaskKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::world::tile::TileKind;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{egui, EguiContexts};

/// An entity found on the right-clicked tile that is displayed in Section 2.
struct TileEntityInfo {
    entity: Entity,
    display_name: String,
    hostility: Hostility,
    health: Option<(u8, u8)>,
    is_corpse: bool,
}

/// A ground-item stack found on the right-clicked tile, displayed in Section 3.
struct TileItemInfo {
    entity: Entity,
    item: Item,
    qty: u32,
}

#[derive(Resource, Default)]
pub struct ContextMenuState {
    pub open: bool,
    pub screen_pos: egui::Pos2,
    pub target_tile: (i16, i16),
    /// Foot Z of the targeted tile at the moment of right-click.
    pub target_z: i8,
    /// Top-level tile actions shown directly (Move, Mine, Gather, …).
    pub actions: Vec<PlayerOrderKind>,
    /// Build options nested under the "Build ▸" submenu. `bool` is whether the
    /// player faction has the required tech — locked options render greyed-out.
    pub build_options: Vec<(PlayerOrderKind, bool)>,
    /// Non-item entities on the target tile (Section 2).
    pub tile_entities: Vec<TileEntityInfo>,
    /// Ground-item stacks on the target tile (Section 3).
    pub tile_items: Vec<TileItemInfo>,
}

impl ContextMenuState {
    fn clear_tile_data(&mut self) {
        self.tile_entities.clear();
        self.tile_items.clear();
    }
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

/// Bundled queries used by `right_click_context_menu_system` so the system
/// fits under Bevy's 16-param ceiling.
#[derive(SystemParam)]
pub struct OrderMemberQueries<'w, 's> {
    pub drafted_q: Query<'w, 's, (), With<Drafted>>,
    pub faction_q: Query<'w, 's, &'static FactionMember>,
}

/// Read-only queries for classifying and displaying entities at the target tile.
#[derive(SystemParam)]
pub struct TileDisplayQueries<'w, 's> {
    pub ground_items_q: Query<'w, 's, (Entity, &'static GroundItem)>,
    pub health_q: Query<'w, 's, &'static Health>,
    pub name_q: Query<'w, 's, &'static Name>,
    pub person_q: Query<'w, 's, (), With<Person>>,
    pub wolf_q: Query<'w, 's, (), With<Wolf>>,
    pub deer_q: Query<'w, 's, (), With<Deer>>,
    pub fox_q: Query<'w, 's, (), With<Fox>>,
    pub corpse_q: Query<'w, 's, &'static Corpse>,
    pub profession_q: Query<'w, 's, &'static Profession>,
}

/// All routing resources bundled to stay under the 16-param system limit.
#[derive(SystemParam)]
pub struct RoutingResources<'w, 's> {
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub camera_view_z: Res<'w, CameraViewZ>,
    pub bed_map: Res<'w, BedMap>,
    pub campfire_map: Res<'w, CampfireMap>,
    pub door_map: Res<'w, DoorMap>,
    pub table_map: Res<'w, TableMap>,
    pub chair_map: Res<'w, ChairMap>,
    pub workbench_map: Res<'w, WorkbenchMap>,
    pub loom_map: Res<'w, LoomMap>,
    pub bp_map: ResMut<'w, BlueprintMap>,
    #[system_param(ignore)]
    pub _marker: std::marker::PhantomData<&'s ()>,
}

pub fn right_click_context_menu_system(
    mut contexts: EguiContexts,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<Camera2d>>,
    selected: Res<SelectedEntity>,
    member_q: OrderMemberQueries,
    player_faction: Res<PlayerFaction>,
    faction_registry: Res<FactionRegistry>,
    mut ai_q: Query<(&mut PersonAI, &Transform, &mut CombatTarget, &mut TargetItem)>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    spatial: Res<SpatialIndex>,
    tile_display: TileDisplayQueries,
    mut routing: RoutingResources,
    mut menu_state: ResMut<ContextMenuState>,
    mut commands: Commands,
) {
    // Require a selected player-faction member.
    let Some(sel_entity) = selected.0 else {
        menu_state.open = false;
        return;
    };
    let is_player_member = member_q
        .faction_q
        .get(sel_entity)
        .map(|m| m.faction_id == player_faction.faction_id)
        .unwrap_or(false);
    if !is_player_member {
        menu_state.open = false;
        return;
    }
    // Drafted units are commanded by `military_right_click_system` instead.
    if member_q.drafted_q.get(sel_entity).is_ok() {
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

                    let underground = routing.camera_view_z.0 != i32::MAX;
                    let target_z_i32 = if underground {
                        routing.camera_view_z.0
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

                    let player_techs: FactionTechs = member_q
                        .faction_q
                        .get(sel_entity)
                        .ok()
                        .and_then(|m| faction_registry.factions.get(&m.faction_id))
                        .map(|f| f.techs.clone())
                        .unwrap_or_default();

                    let pos_tile = (tx as i16, ty as i16);
                    let already_built = routing.bed_map.0.contains_key(&pos_tile)
                        || routing.campfire_map.0.contains_key(&pos_tile)
                        || routing.door_map.0.contains_key(&pos_tile)
                        || routing.table_map.0.contains_key(&pos_tile)
                        || routing.chair_map.0.contains_key(&pos_tile)
                        || routing.workbench_map.0.contains_key(&pos_tile)
                        || routing.loom_map.0.contains_key(&pos_tile);

                    if let Some(kind) = target_kind {
                        if matches!(kind, TileKind::Wall | TileKind::Stone) {
                            actions.push(PlayerOrderKind::Mine);
                        }
                        if kind.is_passable() && !underground {
                            actions.push(PlayerOrderKind::DigDown);
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

                    // Populate tile entities and items (Sections 2 & 3).
                    menu_state.clear_tile_data();
                    for &e in spatial.get(tx, ty) {
                        if e == sel_entity {
                            continue;
                        }
                        if let Ok((item_entity, gi)) = tile_display.ground_items_q.get(e) {
                            menu_state.tile_items.push(TileItemInfo {
                                entity: item_entity,
                                item: gi.item,
                                qty: gi.qty,
                            });
                        } else {
                            let hostility = classify_target(
                                e,
                                player_faction.faction_id,
                                &faction_registry,
                                &member_q.faction_q,
                                &tile_display.wolf_q,
                                &tile_display.fox_q,
                            );
                            let health = tile_display
                                .health_q
                                .get(e)
                                .ok()
                                .map(|h| (h.current, h.max));
                            let is_corpse = tile_display.corpse_q.get(e).is_ok();
                            let display_name = entity_display_name(
                                e,
                                &tile_display.name_q,
                                &tile_display.person_q,
                                &tile_display.profession_q,
                                &tile_display.wolf_q,
                                &tile_display.deer_q,
                                &tile_display.fox_q,
                                &tile_display.corpse_q,
                            );
                            menu_state.tile_entities.push(TileEntityInfo {
                                entity: e,
                                display_name,
                                hostility,
                                health,
                                is_corpse,
                            });
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
    // Clone enough display data to use in the closure without borrow issues.
    let tile_entities: Vec<(Entity, String, Hostility, Option<(u8, u8)>, bool)> = menu_state
        .tile_entities
        .iter()
        .map(|e| {
            (
                e.entity,
                e.display_name.clone(),
                e.hostility,
                e.health,
                e.is_corpse,
            )
        })
        .collect();
    let tile_items: Vec<(Entity, Item, u32)> = menu_state
        .tile_items
        .iter()
        .map(|i| (i.entity, i.item, i.qty))
        .collect();
    let mut chosen: Option<PlayerOrderKind> = None;

    egui::Area::new("context_menu".into())
        .fixed_pos(menu_state.screen_pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                // Section 1 — Tile actions (Move, Mine, Gather, Dig, Deconstruct)
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

                // Section 2 — Entities on tile
                if !tile_entities.is_empty() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("── Entities ──")
                            .small()
                            .color(egui::Color32::from_gray(160)),
                    );
                    for (entity, name, hostility, health, is_corpse) in &tile_entities {
                        let info_label = if let Some((cur, max)) = health {
                            format!("{name}  \u{2665}{cur}/{max}")
                        } else {
                            name.clone()
                        };
                        ui.horizontal(|ui| {
                            ui.label(&info_label);
                            if *hostility != Hostility::Friendly && health.is_some() {
                                if ui.small_button("Attack").clicked() {
                                    chosen = Some(PlayerOrderKind::AttackEntity(*entity));
                                }
                            }
                            if *is_corpse {
                                if ui.small_button("Pick up corpse").clicked() {
                                    chosen = Some(PlayerOrderKind::PickUpCorpse(*entity));
                                }
                            }
                        });
                    }
                }

                // Section 3 — Items on tile
                if !tile_items.is_empty() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("── Items ──")
                            .small()
                            .color(egui::Color32::from_gray(160)),
                    );
                    for (entity, item, qty) in &tile_items {
                        let label = format!("Pick up: {qty}\u{00d7} {}", item.label());
                        if ui.button(&label).clicked() {
                            chosen = Some(PlayerOrderKind::PickUpItem(*entity));
                        }
                    }
                }
            });
        });

    if let Some(action) = chosen {
        if let Ok((mut ai, transform, mut combat_target, mut target_item)) =
            ai_q.get_mut(sel_entity)
        {
            let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let cur_chunk = ChunkCoord(
                cur_tx.div_euclid(CHUNK_SIZE as i32),
                cur_ty.div_euclid(CHUNK_SIZE as i32),
            );

            // For Build orders: spawn a personal Blueprint at the target tile.
            let build_bp: Option<Entity> = if let PlayerOrderKind::Build(kind) = action {
                if !routing.bp_map.0.contains_key(&target_tile) {
                    let faction_id = member_q
                        .faction_q
                        .get(sel_entity)
                        .map(|m| m.faction_id)
                        .unwrap_or(SOLO);
                    let wp = tile_to_world(target_tile.0 as i32, target_tile.1 as i32);
                    let bz =
                        chunk_map.surface_z_at(target_tile.0 as i32, target_tile.1 as i32) as i8;
                    let bp_e = commands
                        .spawn((
                            Blueprint::new(
                                faction_id,
                                Some(sel_entity),
                                kind,
                                target_tile,
                                bz,
                            ),
                            Transform::from_xyz(wp.x, wp.y, 0.3),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    routing.bp_map.0.insert(target_tile, bp_e);
                    Some(bp_e)
                } else {
                    routing.bp_map.0.get(&target_tile).copied()
                }
            } else {
                None
            };

            match action {
                PlayerOrderKind::Move => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Idle,
                        None,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                    ai.target_z = target_z;
                }
                PlayerOrderKind::Mine | PlayerOrderKind::Gather => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Gather,
                        None,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::PickUp => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Scavenge,
                        None,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::PickUpItem(item_entity) => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Scavenge,
                        Some(item_entity),
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                    target_item.0 = Some(item_entity);
                }
                PlayerOrderKind::AttackEntity(foe) => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::MilitaryAttack,
                        Some(foe),
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                    combat_target.0 = None;
                }
                PlayerOrderKind::PickUpCorpse(corpse_entity) => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::PickUpCorpse,
                        Some(corpse_entity),
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::Build(BuildSiteKind::Bed) => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::ConstructBed,
                        build_bp,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::Build(_) => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Construct,
                        build_bp,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::DigDown => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Dig,
                        None,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
                PlayerOrderKind::Deconstruct => {
                    assign_task_with_routing(
                        &mut ai,
                        (cur_tx as i16, cur_ty as i16),
                        cur_chunk,
                        target_tile,
                        TaskKind::Deconstruct,
                        None,
                        &routing.chunk_graph,
                        &routing.chunk_router,
                        &chunk_map,
                        &routing.chunk_connectivity,
                    );
                }
            }

            // For non-adjacent tasks (Move) honor the player's chosen Z layer.
            let task = ai.task_id;
            if !task_interacts_from_adjacent(task) {
                ai.target_z = target_z;
            }
        }
        commands.entity(sel_entity).insert(PlayerOrder {
            order: action,
            target_tile,
            target_z,
        });
        menu_state.open = false;
    }
}

/// Build a human-readable display name for an entity on the right-clicked tile.
fn entity_display_name(
    entity: Entity,
    name_q: &Query<&Name>,
    person_q: &Query<(), With<Person>>,
    profession_q: &Query<&Profession>,
    wolf_q: &Query<(), With<Wolf>>,
    deer_q: &Query<(), With<Deer>>,
    fox_q: &Query<(), With<Fox>>,
    corpse_q: &Query<&Corpse>,
) -> String {
    if let Ok(corpse) = corpse_q.get(entity) {
        return format!("{:?} Corpse", corpse.species);
    }
    if person_q.get(entity).is_ok() {
        let name = name_q
            .get(entity)
            .map(|n| n.as_str())
            .unwrap_or("Person");
        let profession = profession_q.get(entity).ok();
        return match profession {
            Some(Profession::Farmer) => format!("{name} (Farmer)"),
            Some(Profession::Hunter) => format!("{name} (Hunter)"),
            _ => name.to_owned(),
        };
    }
    if wolf_q.get(entity).is_ok() {
        return "Wolf".to_owned();
    }
    if deer_q.get(entity).is_ok() {
        return "Deer".to_owned();
    }
    if fox_q.get(entity).is_ok() {
        return "Fox".to_owned();
    }
    name_q
        .get(entity)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "Unknown".to_owned())
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

/// Persistent state for the small two-button popup shown when drafted units
/// right-click a *neutral* entity (foreign faction not at war, passive
/// animal). For hostile or empty-tile right-clicks, no popup is shown — the
/// order resolves immediately.
#[derive(Resource, Default)]
pub struct MilitaryMenuState {
    pub open: bool,
    pub screen_pos: egui::Pos2,
    pub target_entity: Option<Entity>,
    pub target_tile: (i16, i16),
    pub target_z: i8,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Hostility {
    Friendly,
    Hostile,
    Neutral,
}

fn classify_target(
    target: Entity,
    player_faction_id: u32,
    registry: &FactionRegistry,
    faction_q: &Query<&FactionMember>,
    wolf_q: &Query<(), With<Wolf>>,
    fox_q: &Query<(), With<Fox>>,
) -> Hostility {
    if let Ok(member) = faction_q.get(target) {
        if member.faction_id == player_faction_id {
            return Hostility::Friendly;
        }
        let other = member.faction_id;
        let pf = registry.factions.get(&player_faction_id);
        let of = registry.factions.get(&other);
        let at_war = pf
            .and_then(|f| f.raid_target)
            .map(|t| t == other)
            .unwrap_or(false)
            || of
                .and_then(|f| f.raid_target)
                .map(|t| t == player_faction_id)
                .unwrap_or(false);
        return if at_war {
            Hostility::Hostile
        } else {
            Hostility::Neutral
        };
    }
    // Predator animals are auto-hostile; everyone else (passive animals,
    // unknown entities) is treated as neutral.
    if wolf_q.get(target).is_ok() || fox_q.get(target).is_ok() {
        return Hostility::Hostile;
    }
    Hostility::Neutral
}

#[derive(SystemParam)]
pub struct MilitaryRouting<'w, 's> {
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub chunk_map: Res<'w, ChunkMap>,
    #[system_param(ignore)]
    pub _marker: std::marker::PhantomData<&'s ()>,
}

/// Read-only queries used by the military right-click classifier.
#[derive(SystemParam)]
pub struct ClassifyQueries<'w, 's> {
    pub faction_q: Query<'w, 's, &'static FactionMember>,
    pub wolf_q: Query<'w, 's, (), With<Wolf>>,
    pub fox_q: Query<'w, 's, (), With<Fox>>,
    pub person_q: Query<'w, 's, (), With<Person>>,
    pub health_q: Query<'w, 's, &'static Health>,
    pub transform_q: Query<'w, 's, &'static Transform>,
    pub drafted_q: Query<'w, 's, (), With<Drafted>>,
}

/// Issue a `MilitaryMove` to every drafted player-faction unit in
/// `drafted_units`, registering the destination tile as a flow-field
/// hotspot so units in the goal chunk skip per-agent A*.
fn issue_group_move(
    drafted_units: &[Entity],
    target_tile: (i16, i16),
    target_z: i8,
    routing: &MilitaryRouting,
    ai_q: &mut Query<(&mut PersonAI, &Transform, &mut CombatTarget), With<Drafted>>,
    hotspots: &mut HotspotFlowFields,
) {
    hotspots.register(
        (target_tile.0, target_tile.1, target_z),
        HotspotKind::RallyPoint,
    );
    for &e in drafted_units {
        let Ok((mut ai, transform, mut combat)) = ai_q.get_mut(e) else {
            continue;
        };
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        assign_task_with_routing(
            &mut ai,
            (cur_tx as i16, cur_ty as i16),
            cur_chunk,
            target_tile,
            TaskKind::MilitaryMove,
            None,
            &routing.chunk_graph,
            &routing.chunk_router,
            &routing.chunk_map,
            &routing.chunk_connectivity,
        );
        ai.target_z = target_z;
        combat.0 = None;
    }
}

/// Issue a `MilitaryAttack` against `foe` to every drafted unit. Each unit
/// routes toward the foe's current tile; the attack-task driver re-targets
/// each tick as the foe moves.
fn issue_group_attack(
    drafted_units: &[Entity],
    foe: Entity,
    foe_tile: (i16, i16),
    foe_z: i8,
    routing: &MilitaryRouting,
    ai_q: &mut Query<(&mut PersonAI, &Transform, &mut CombatTarget), With<Drafted>>,
    hotspots: &mut HotspotFlowFields,
) {
    hotspots.register((foe_tile.0, foe_tile.1, foe_z), HotspotKind::RallyPoint);
    for &e in drafted_units {
        if e == foe {
            continue;
        }
        let Ok((mut ai, transform, mut combat)) = ai_q.get_mut(e) else {
            continue;
        };
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        assign_task_with_routing(
            &mut ai,
            (cur_tx as i16, cur_ty as i16),
            cur_chunk,
            foe_tile,
            TaskKind::MilitaryAttack,
            Some(foe),
            &routing.chunk_graph,
            &routing.chunk_router,
            &routing.chunk_map,
            &routing.chunk_connectivity,
        );
        combat.0 = None; // attack-task driver sets it on adjacency
    }
}

pub fn military_right_click_system(
    mut contexts: EguiContexts,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<Camera2d>>,
    selected_many: Res<SelectedEntities>,
    player_faction: Res<PlayerFaction>,
    classify: ClassifyQueries,
    spatial: Res<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    camera_view_z: Res<CameraViewZ>,
    registry: Res<FactionRegistry>,
    routing: MilitaryRouting,
    mut ai_q: Query<(&mut PersonAI, &Transform, &mut CombatTarget), With<Drafted>>,
    mut hotspots: ResMut<HotspotFlowFields>,
    mut menu_state: ResMut<MilitaryMenuState>,
) {
    // Snapshot drafted player-faction members from the selection.
    let drafted_units: Vec<Entity> = selected_many
        .ids
        .iter()
        .copied()
        .filter(|e| classify.drafted_q.get(*e).is_ok())
        .filter(|e| {
            classify
                .faction_q
                .get(*e)
                .map(|m| m.faction_id == player_faction.faction_id)
                .unwrap_or(false)
        })
        .collect();

    if drafted_units.is_empty() {
        menu_state.open = false;
        return;
    }

    let ctx = contexts.ctx_mut();

    // Right-click: classify and either resolve immediately or open the
    // neutral popup.
    if !ctx.is_pointer_over_area() && mouse_buttons.just_pressed(MouseButton::Right) {
        let (Ok(window), Ok((camera, cam_transform))) =
            (windows.get_single(), camera_q.get_single())
        else {
            return;
        };
        let Some(cursor_pos) = window.cursor_position() else {
            return;
        };
        let Ok(world_pos) = camera.viewport_to_world_2d(cam_transform, cursor_pos) else {
            return;
        };
        let tx = (world_pos.x / TILE_SIZE).floor() as i32;
        let ty = (world_pos.y / TILE_SIZE).floor() as i32;
        let underground = camera_view_z.0 != i32::MAX;
        let target_z = if underground {
            camera_view_z.0 as i8
        } else {
            chunk_map.surface_z_at(tx, ty) as i8
        };

        // Find a candidate target entity at this tile: prefer Persons (other
        // faction members) and animals, ignore items/blueprints/etc. Pick the
        // nearest by world-space distance to the cursor.
        let mut best: Option<(Entity, f32)> = None;
        for &e in spatial.get(tx, ty) {
            let is_unit = classify.person_q.get(e).is_ok()
                || classify.wolf_q.get(e).is_ok()
                || classify.fox_q.get(e).is_ok();
            if !is_unit {
                continue;
            }
            let Ok(t) = classify.transform_q.get(e) else {
                continue;
            };
            let d = t.translation.truncate().distance(world_pos);
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((e, d));
            }
        }

        let target = best.map(|(e, _)| e);
        let target_tile_i16 = (tx as i16, ty as i16);

        match target {
            None => {
                // Empty tile → group move.
                issue_group_move(
                    &drafted_units,
                    target_tile_i16,
                    target_z,
                    &routing,
                    &mut ai_q,
                    &mut hotspots,
                );
                menu_state.open = false;
            }
            Some(foe) => {
                let class = classify_target(
                    foe,
                    player_faction.faction_id,
                    &registry,
                    &classify.faction_q,
                    &classify.wolf_q,
                    &classify.fox_q,
                );
                let foe_tile = classify
                    .transform_q
                    .get(foe)
                    .map(|t| {
                        (
                            (t.translation.x / TILE_SIZE).floor() as i16,
                            (t.translation.y / TILE_SIZE).floor() as i16,
                        )
                    })
                    .unwrap_or(target_tile_i16);
                match class {
                    Hostility::Friendly => {
                        // Right-clicking your own unit is a no-op.
                        menu_state.open = false;
                    }
                    Hostility::Hostile => {
                        if classify.health_q.get(foe).is_ok() {
                            issue_group_attack(
                                &drafted_units,
                                foe,
                                foe_tile,
                                target_z,
                                &routing,
                                &mut ai_q,
                                &mut hotspots,
                            );
                        }
                        menu_state.open = false;
                    }
                    Hostility::Neutral => {
                        menu_state.open = true;
                        menu_state.screen_pos = egui::pos2(cursor_pos.x, cursor_pos.y);
                        menu_state.target_entity = Some(foe);
                        menu_state.target_tile = foe_tile;
                        menu_state.target_z = target_z;
                    }
                }
            }
        }
    }

    // Close the neutral popup on left-click outside.
    if menu_state.open
        && !ctx.is_pointer_over_area()
        && mouse_buttons.just_pressed(MouseButton::Left)
    {
        menu_state.open = false;
    }

    if !menu_state.open {
        return;
    }

    let mut chosen_attack = false;
    let mut chosen_move = false;
    egui::Area::new("military_menu".into())
        .fixed_pos(menu_state.screen_pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                if ui.button("Attack").clicked() {
                    chosen_attack = true;
                }
                if ui.button("Move here").clicked() {
                    chosen_move = true;
                }
            });
        });

    if chosen_attack {
        if let Some(foe) = menu_state.target_entity {
            if classify.health_q.get(foe).is_ok() {
                issue_group_attack(
                    &drafted_units,
                    foe,
                    menu_state.target_tile,
                    menu_state.target_z,
                    &routing,
                    &mut ai_q,
                    &mut hotspots,
                );
            }
        }
        menu_state.open = false;
    } else if chosen_move {
        issue_group_move(
            &drafted_units,
            menu_state.target_tile,
            menu_state.target_z,
            &routing,
            &mut ai_q,
            &mut hotspots,
        );
        menu_state.open = false;
    }
}
