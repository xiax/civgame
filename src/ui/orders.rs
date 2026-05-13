use super::selection::{SelectedEntities, SelectedEntity};
use crate::economy::item::Item;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::rendering::camera::CameraViewZ;
use crate::simulation::animals::{Cat, Cow, Deer, Fox, Horse, Pig, Tamed, Wolf};
use crate::simulation::combat::{CombatTarget, Health};
use crate::simulation::construction::{
    faction_can_build, recipe_for, BarracksMap, BedMap, Blueprint, BlueprintMap, BuildSiteKind,
    CampfireMap, ChairMap, DoorMap, GranaryMap, LoomMap, MarketMap, MonumentMap, ShrineMap,
    TableMap, WallMaterial, WorkbenchMap,
};
use crate::simulation::corpse::Corpse;
use crate::simulation::faction::SOLO;
use crate::simulation::faction::{FactionMember, FactionRegistry, FactionTechs, PlayerFaction};
use crate::simulation::items::{GroundItem, TargetItem};
use crate::simulation::person::{Drafted, Person, Profession};
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

/// UI-internal selection enum for the right-click menu. Captures which button
/// the player clicked; the menu code attaches the right-click target tile/z
/// from `ContextMenuState` and constructs the corresponding `PlayerCommand`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Move,
    Mine,
    Gather,
    PickUp,
    Build(crate::simulation::construction::BuildSiteKind),
    DigDown,
    Deconstruct,
    /// Pick up a specific `GroundItem` entity.
    PickUpItem(Entity),
    /// Attack a specific entity (usable by non-drafted workers).
    AttackEntity(Entity),
    /// Pick up a specific fresh `Corpse` entity.
    PickUpCorpse(Entity),
    /// 1-on-1 teaching: the selected agent walks to the target person.
    Teach(Entity),
    /// Player-directed lecture: the selected agent broadcasts to nearby adults.
    HoldLecture(crate::simulation::technology::TechId),
    /// Self-study from a tablet/book in the actor's inventory.
    ReadItem(crate::simulation::technology::TechId),
    /// Faction-level: craft a tablet encoding the given tech.
    EncodeTablet(crate::simulation::technology::TechId),
    /// Pitch the player faction's mobile camp at this tile (only
    /// available when `caps.home.is_mobile()` and `camp_state ==
    /// Packed`). The chief is the dispatched actor; the apply system
    /// re-seeds the camp here and flips state to Pitched.
    PitchCampHere,
}

impl MenuAction {
    pub fn label(self) -> &'static str {
        use crate::simulation::construction::{BuildSiteKind, WallMaterial};
        match self {
            MenuAction::Move => "Move here",
            MenuAction::Mine => "Mine",
            MenuAction::Gather => "Gather",
            MenuAction::PickUp => "Pick up",
            MenuAction::Build(kind) => match kind {
                BuildSiteKind::Wall(WallMaterial::Palisade) => "Build Palisade",
                BuildSiteKind::Wall(WallMaterial::WattleDaub) => "Build Wattle Wall",
                BuildSiteKind::Wall(WallMaterial::Stone) => "Build Stone Wall",
                BuildSiteKind::Wall(WallMaterial::Mudbrick) => "Build Mudbrick Wall",
                BuildSiteKind::Wall(WallMaterial::CutStone) => "Build Cut Stone Wall",
                BuildSiteKind::Door => "Build Door",
                BuildSiteKind::Bed => "Build Bed",
                BuildSiteKind::Bedroll => "Build Bedroll",
                BuildSiteKind::Tent => "Build Tent",
                BuildSiteKind::Yurt => "Build Yurt",
                BuildSiteKind::Campfire => "Build Campfire",
                BuildSiteKind::Workbench => "Build Workbench",
                BuildSiteKind::Loom => "Build Loom",
                BuildSiteKind::Table => "Build Table",
                BuildSiteKind::Chair => "Build Chair",
                BuildSiteKind::Granary => "Build Granary",
                BuildSiteKind::Shrine => "Build Shrine",
                BuildSiteKind::Market => "Build Market",
                BuildSiteKind::Barracks => "Build Barracks",
                BuildSiteKind::Monument => "Build Monument",
            },
            MenuAction::DigDown => "Dig Down",
            MenuAction::Deconstruct => "Deconstruct",
            MenuAction::PickUpItem(_) => "Pick up item",
            MenuAction::AttackEntity(_) => "Attack",
            MenuAction::PickUpCorpse(_) => "Pick up corpse",
            MenuAction::Teach(_) => "Teach",
            MenuAction::HoldLecture(_) => "Hold Lecture",
            MenuAction::ReadItem(_) => "Read",
            MenuAction::EncodeTablet(_) => "Encode Tablet",
            MenuAction::PitchCampHere => "Pitch Camp Here",
        }
    }
}

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
    pub target_tile: (i32, i32),
    /// Foot Z of the targeted tile at the moment of right-click.
    pub target_z: i8,
    /// Top-level tile actions shown directly (Move, Mine, Gather, …).
    pub actions: Vec<MenuAction>,
    /// Build options nested under the "Build ▸" submenu. `bool` is whether the
    /// player faction has the required tech — locked options render greyed-out.
    pub build_options: Vec<(MenuAction, bool)>,
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
fn all_build_options() -> [BuildSiteKind; 17] {
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
        BuildSiteKind::Granary,
        BuildSiteKind::Shrine,
        BuildSiteKind::Market,
        BuildSiteKind::Barracks,
        BuildSiteKind::Monument,
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
    pub horse_q: Query<'w, 's, (), With<Horse>>,
    pub cow_q: Query<'w, 's, (), With<Cow>>,
    pub pig_q: Query<'w, 's, (), With<Pig>>,
    pub cat_q: Query<'w, 's, (), With<Cat>>,
    pub tamed_q: Query<'w, 's, (), With<Tamed>>,
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
    pub granary_map: Res<'w, GranaryMap>,
    pub shrine_map: Res<'w, ShrineMap>,
    pub market_map: Res<'w, MarketMap>,
    pub barracks_map: Res<'w, BarracksMap>,
    pub monument_map: Res<'w, MonumentMap>,
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
    selected_many: Res<SelectedEntities>,
    member_q: OrderMemberQueries,
    player_faction: Res<PlayerFaction>,
    faction_registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    plant_map: Res<PlantMap>,
    spatial: Res<SpatialIndex>,
    tile_display: TileDisplayQueries,
    routing: RoutingResources,
    mut menu_state: ResMut<ContextMenuState>,
    mut cmd_events: EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
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
    let is_drafted = member_q.drafted_q.get(sel_entity).is_ok();
    if !is_player_member {
        menu_state.open = false;
        return;
    }
    // Drafted units are commanded by `military_right_click_system` instead.
    if is_drafted {
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

                    let mut actions = vec![MenuAction::Move];
                    let mut build_options: Vec<(MenuAction, bool)> = Vec::new();

                    // Build-menu unlock check uses the *community-adoption*
                    // bitset, mirroring `chief_directive_system`. A chief
                    // who's merely Aware of bronze doesn't get bronze
                    // walls in their build menu — the village must have
                    // adopted the gating tech first.
                    let player_techs: FactionTechs = member_q
                        .faction_q
                        .get(sel_entity)
                        .ok()
                        .and_then(|m| faction_registry.factions.get(&m.faction_id))
                        .map(|f| {
                            crate::simulation::technology_adoption::community_adoption_bitset(f)
                        })
                        .unwrap_or_default();

                    let pos_tile = (tx as i32, ty as i32);
                    let already_built = routing.bed_map.0.contains_key(&pos_tile)
                        || routing.campfire_map.0.contains_key(&pos_tile)
                        || routing.door_map.0.contains_key(&pos_tile)
                        || routing.table_map.0.contains_key(&pos_tile)
                        || routing.chair_map.0.contains_key(&pos_tile)
                        || routing.workbench_map.0.contains_key(&pos_tile)
                        || routing.loom_map.0.contains_key(&pos_tile)
                        || routing.granary_map.0.contains_key(&pos_tile)
                        || routing.shrine_map.0.contains_key(&pos_tile)
                        || routing.market_map.0.contains_key(&pos_tile)
                        || routing.barracks_map.0.contains_key(&pos_tile)
                        || routing.monument_map.0.contains_key(&pos_tile);

                    if let Some(kind) = target_kind {
                        if matches!(kind, TileKind::Wall | TileKind::Stone) {
                            actions.push(MenuAction::Mine);
                        }
                        if kind.is_passable() && !underground {
                            actions.push(MenuAction::DigDown);
                            if !already_built {
                                for bk in all_build_options() {
                                    let unlocked = faction_can_build(bk, &player_techs);
                                    build_options.push((MenuAction::Build(bk), unlocked));
                                }
                            }
                            // Pitch Camp Here — visible when the player
                            // faction is Packed (mobile, shelters down)
                            // and the tile is reachable + far enough
                            // from the band centroid.
                            if let Some(player_fac) =
                                faction_registry.factions.get(&player_faction.faction_id)
                            {
                                use crate::simulation::faction::CampState;
                                if player_fac.caps.home.is_mobile()
                                    && matches!(player_fac.camp_state, CampState::Packed { .. })
                                {
                                    let cheb = (pos_tile.0 - player_fac.home_tile.0)
                                        .abs()
                                        .max((pos_tile.1 - player_fac.home_tile.1).abs());
                                    if cheb >= crate::simulation::nomad::MIN_PITCH_DISTANCE {
                                        actions.push(MenuAction::PitchCampHere);
                                    }
                                }
                            }
                        }
                    }
                    if !underground && already_built {
                        actions.push(MenuAction::Deconstruct);
                    }
                    if !underground && plant_map.0.contains_key(&(tx, ty)) {
                        actions.push(MenuAction::Gather);
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
                                &tile_display.horse_q,
                                &tile_display.cow_q,
                                &tile_display.pig_q,
                                &tile_display.cat_q,
                                &tile_display.tamed_q,
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
    let mut chosen: Option<MenuAction> = None;

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
                                MenuAction::Build(bk) => recipe_for(*bk).name,
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
                                    chosen = Some(MenuAction::AttackEntity(*entity));
                                }
                            }
                            if *is_corpse {
                                if ui.small_button("Pick up corpse").clicked() {
                                    chosen = Some(MenuAction::PickUpCorpse(*entity));
                                }
                            }
                            // Teach: friendly person target, distinct from
                            // selected. Eligibility (teacher has any teachable
                            // tech) is verified by `apply_teach_order_system`.
                            if *hostility == Hostility::Friendly
                                && !*is_corpse
                                && *entity != sel_entity
                                && health.is_some()
                            {
                                if ui.small_button("Teach").clicked() {
                                    chosen = Some(MenuAction::Teach(*entity));
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
                            chosen = Some(MenuAction::PickUpItem(*entity));
                        }
                    }
                }
            });
        });

    if let Some(action) = chosen {
        if let Some(cmd) = menu_action_to_command(action, target_tile, target_z) {
            // Broadcast to every non-drafted player-faction worker in
            // `SelectedEntities`. Drafted units are owned by
            // `military_right_click_system`; filtering them here avoids
            // double-commanding hunters who happen to be in the drag-rect.
            let mut actors: Vec<Entity> = selected_many
                .ids
                .iter()
                .copied()
                .filter(|&e| {
                    let is_player = member_q
                        .faction_q
                        .get(e)
                        .map(|m| m.faction_id == player_faction.faction_id)
                        .unwrap_or(false);
                    let is_drafted = member_q.drafted_q.get(e).is_ok();
                    is_player && !is_drafted
                })
                .collect();
            if actors.is_empty() {
                actors.push(sel_entity);
            }
            cmd_events.send(crate::simulation::player_command::PlayerCommandEvent {
                actors,
                command: cmd,
            });
        }
        menu_state.open = false;
    }
}

/// Translate a UI `MenuAction` selection (plus the right-clicked target
/// tile / z) into the equivalent `PlayerCommand` event payload. Returning
/// `None` means the UI still needs to take a legacy path (currently nothing —
/// all UI paths now have a `PlayerCommand` equivalent).
fn menu_action_to_command(
    action: MenuAction,
    target_tile: (i32, i32),
    target_z: i8,
) -> Option<crate::simulation::player_command::PlayerCommand> {
    use crate::simulation::player_command::PlayerCommand;
    Some(match action {
        MenuAction::Move => PlayerCommand::Move {
            tile: target_tile,
            z: target_z,
        },
        MenuAction::Gather => PlayerCommand::Gather {
            tile: target_tile,
            z: target_z,
        },
        MenuAction::Mine => PlayerCommand::Mine {
            tile: target_tile,
            z: target_z,
        },
        MenuAction::Build(kind) => PlayerCommand::Build {
            kind,
            tile: target_tile,
            z: target_z,
        },
        MenuAction::Deconstruct => PlayerCommand::Deconstruct {
            tile: target_tile,
            z: target_z,
        },
        MenuAction::DigDown => PlayerCommand::DigDown {
            tile: target_tile,
            z: target_z,
        },
        MenuAction::PickUpItem(item) => PlayerCommand::PickUpItem {
            item,
            tile: target_tile,
            z: target_z,
        },
        MenuAction::PickUpCorpse(corpse) => PlayerCommand::PickUpCorpse {
            corpse,
            tile: target_tile,
            z: target_z,
        },
        MenuAction::AttackEntity(foe) => PlayerCommand::AttackEntity {
            foe,
            tile: target_tile,
            z: target_z,
        },
        MenuAction::Teach(student) => PlayerCommand::Teach {
            student,
            tile: target_tile,
            z: target_z,
        },
        MenuAction::HoldLecture(tech) => PlayerCommand::HoldLecture { tech },
        MenuAction::ReadItem(tech) => PlayerCommand::ReadItem { tech },
        MenuAction::EncodeTablet(tech) => PlayerCommand::EncodeTablet { tech },
        MenuAction::PitchCampHere => PlayerCommand::PitchCamp {
            tile: target_tile,
            z: target_z,
        },
        // `PickUp` (generic pick-up) is the menu shortcut that doesn't pre-pick
        // an item entity; UI converts it to Gather/Scavenge based on what's on
        // the tile elsewhere. Treat as Gather here to keep the menu working.
        MenuAction::PickUp => PlayerCommand::Gather {
            tile: target_tile,
            z: target_z,
        },
    })
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
    horse_q: &Query<(), With<Horse>>,
    cow_q: &Query<(), With<Cow>>,
    pig_q: &Query<(), With<Pig>>,
    cat_q: &Query<(), With<Cat>>,
    tamed_q: &Query<(), With<Tamed>>,
    corpse_q: &Query<&Corpse>,
) -> String {
    if let Ok(corpse) = corpse_q.get(entity) {
        return format!("{:?} Corpse", corpse.species);
    }
    if person_q.get(entity).is_ok() {
        let name = name_q.get(entity).map(|n| n.as_str()).unwrap_or("Person");
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
    if horse_q.get(entity).is_ok() {
        return if tamed_q.get(entity).is_ok() {
            "Horse (tamed)".to_owned()
        } else {
            "Horse".to_owned()
        };
    }
    if cow_q.get(entity).is_ok() {
        return "Cow".to_owned();
    }
    if pig_q.get(entity).is_ok() {
        return "Pig".to_owned();
    }
    if cat_q.get(entity).is_ok() {
        return "Cat".to_owned();
    }
    name_q
        .get(entity)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "Unknown".to_owned())
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
    pub target_tile: (i32, i32),
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
    mut menu_state: ResMut<MilitaryMenuState>,
    mut cmd_events: EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
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
        let target_tile_i32 = (tx as i32, ty as i32);

        match target {
            None => {
                // Empty tile → group move.
                emit_military_move(&mut cmd_events, &drafted_units, target_tile_i32, target_z);
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
                            (t.translation.x / TILE_SIZE).floor() as i32,
                            (t.translation.y / TILE_SIZE).floor() as i32,
                        )
                    })
                    .unwrap_or(target_tile_i32);
                match class {
                    Hostility::Friendly => {
                        // Right-clicking your own unit is a no-op.
                        menu_state.open = false;
                    }
                    Hostility::Hostile => {
                        if classify.health_q.get(foe).is_ok() {
                            emit_military_attack(
                                &mut cmd_events,
                                &drafted_units,
                                foe,
                                foe_tile,
                                target_z,
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
                emit_military_attack(
                    &mut cmd_events,
                    &drafted_units,
                    foe,
                    menu_state.target_tile,
                    menu_state.target_z,
                );
            }
        }
        menu_state.open = false;
    } else if chosen_move {
        emit_military_move(
            &mut cmd_events,
            &drafted_units,
            menu_state.target_tile,
            menu_state.target_z,
        );
        menu_state.open = false;
    }
}

fn emit_military_move(
    cmd_events: &mut EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
    actors: &[Entity],
    tile: (i32, i32),
    z: i8,
) {
    use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
    cmd_events.send(PlayerCommandEvent {
        actors: actors.to_vec(),
        command: PlayerCommand::MilitaryMove { tile, z },
    });
}

fn emit_military_attack(
    cmd_events: &mut EventWriter<crate::simulation::player_command::PlayerCommandEvent>,
    actors: &[Entity],
    foe: Entity,
    tile: (i32, i32),
    z: i8,
) {
    use crate::simulation::player_command::{PlayerCommand, PlayerCommandEvent};
    // Drop the foe from the actor list so they don't try to attack themselves.
    let filtered: Vec<Entity> = actors.iter().copied().filter(|&e| e != foe).collect();
    cmd_events.send(PlayerCommandEvent {
        actors: filtered,
        command: PlayerCommand::MilitaryAttack { foe, tile, z },
    });
}
