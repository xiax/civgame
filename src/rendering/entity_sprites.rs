use crate::rendering::pixel_art::{ArtMode, EntityTextures};
use crate::rendering::sprite_library::SpriteLibrary;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::construction::{Bed, Blueprint, BuildSiteKind, Wall};
use crate::simulation::faction::{
    FactionCenter, FactionMember, PlayerFaction, PlayerFactionMarker,
};
use crate::simulation::items::{Equipment, EquipmentSlot};
use crate::simulation::person::{HairColor, Person, PersonAI, SkinTone};
use crate::simulation::plants::{GrowthStage, Plant, PlantKind};
use crate::simulation::reproduction::BiologicalSex;
use crate::world::terrain::tile_to_world;
use bevy::prelude::*;

use bevy::sprite::Anchor;

/// Note: All entity sprites in this game follow a unified alignment rule:
/// 1. Logical entities are spawned at the mathematical center of their tile (wx, wy).
/// 2. Sprites are attached as children with `Anchor::BottomCenter`.
/// 3. To align the sprite's bottom edge with the tile's visual bottom,
///    a universal Y-offset of -8.0 is applied to the child transform.
/// This ensures that 16px tall walls and 36px tall people both stand on the same floor.

#[derive(Component, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntityFogState {
    #[default]
    Visible,
    Explored,
}

#[derive(Component)]
pub struct BedVisual;

#[derive(Component)]
pub struct WallVisual;

#[derive(Component)]
pub struct WolfVisual;

#[derive(Component)]
pub struct DeerVisual;

#[derive(Component)]
pub struct PersonVisual;

/// Floating name label Text2d child. Does NOT carry VisualChild — fog-tint and art-mode systems skip it.
#[derive(Component)]
pub struct PersonNameLabel;

#[derive(Component)]
pub struct FactionCenterVisual;

#[derive(Component)]
pub struct PlantVisual;

#[derive(Component)]
pub struct BlueprintVisual;

/// Base tint color on a VisualChild entity — used by the fog system to preserve
/// sex-based coloring while still applying fog darkening.
#[derive(Component, Clone, Copy)]
pub struct AnimalSexTint(pub Color);

#[derive(Component)]
pub struct VisualChild;

/// Identifies which rendering layer a VisualChild belongs to.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum VisualLayer {
    Body,
    Clothing,
    Hair,
}

/// Cached clothing color key derived from equipped TorsoArmor material.
#[derive(Component, Clone)]
pub struct ClothingVisual {
    pub color_key: &'static str,
    pub visible: bool,
}

impl Default for ClothingVisual {
    fn default() -> Self {
        Self { color_key: "tan", visible: false }
    }
}

/// Tracks the previous-frame world position for direction inference on non-person entities.
#[derive(Component, Default)]
pub struct LastPos(pub Vec2);

#[derive(Component, Clone, Copy, Default, PartialEq, Eq)]
pub enum FacingDirection {
    #[default]
    South,
    North,
    East,
    West,
}

impl FacingDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::South => "s",
            Self::North => "n",
            Self::East => "e",
            Self::West => "w",
        }
    }
}

pub fn spawn_bed_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Bed>, Without<BedVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let img = textures.bed_ascii.clone();

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert((BedVisual, EntityFogState::default()));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_wall_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Wall>, Without<WallVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let img = textures.wall_ascii.clone();

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert((WallVisual, EntityFogState::default()));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_faction_center_sprites(
    mut commands: Commands,
    query: Query<
        (Entity, Option<&PlayerFactionMarker>),
        (With<FactionCenter>, Without<FactionCenterVisual>),
    >,
    textures: Res<EntityTextures>,
) {
    for (entity, player_marker) in query.iter() {
        let img = textures.camp_ascii.clone();

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        if player_marker.is_some() {
            sprite.color = Color::srgb(0.55, 0.85, 1.0);
        }

        commands.entity(entity).insert((FactionCenterVisual, EntityFogState::default()));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_wolf_sprites(
    mut commands: Commands,
    query: Query<
        (Entity, Option<&crate::simulation::reproduction::BiologicalSex>),
        (With<Wolf>, Without<WolfVisual>),
    >,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Ascii {
            textures.wolf_ascii.clone()
        } else {
            sprite_lib.get("anim_wolf_s_a")
                .cloned()
                .unwrap_or_else(|| textures.wolf_ascii.clone())
        };

        // Male wolves are slightly grey; females are reference white
        let tint = match sex_opt {
            Some(crate::simulation::reproduction::BiologicalSex::Female) => Color::WHITE,
            _ => Color::srgb(0.75, 0.75, 0.75),
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;

        commands.entity(entity).insert((
            WolfVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_deer_sprites(
    mut commands: Commands,
    query: Query<
        (Entity, Option<&crate::simulation::reproduction::BiologicalSex>),
        (With<Deer>, Without<DeerVisual>),
    >,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
) {
    for (entity, sex_opt) in query.iter() {
        let img = if *art_mode == ArtMode::Pixel {
            sprite_lib.get("anim_deer_s_a")
                .cloned()
                .unwrap_or_else(|| textures.deer_ascii.clone())
        } else {
            textures.deer_ascii.clone()
        };

        // Male deer are warm tan; females are lighter cream
        let tint = match sex_opt {
            Some(crate::simulation::reproduction::BiologicalSex::Male) | None => {
                Color::srgb(0.80, 0.65, 0.48)
            }
            Some(crate::simulation::reproduction::BiologicalSex::Female) => {
                Color::srgb(0.95, 0.88, 0.78)
            }
        };

        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;
        sprite.color = tint;

        commands.entity(entity).insert((
            DeerVisual,
            EntityFogState::default(),
            FacingDirection::South,
            LastPos::default(),
        ));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                AnimalSexTint(tint),
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn spawn_person_sprites(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&BiologicalSex>,
            Option<&SkinTone>,
            Option<&HairColor>,
            Option<&ClothingVisual>,
            Option<&Name>,
            Option<&FactionMember>,
        ),
        (With<Person>, Without<PersonVisual>),
    >,
    textures: Res<EntityTextures>,
    sprite_lib: Res<SpriteLibrary>,
    art_mode: Res<ArtMode>,
    player_faction: Res<PlayerFaction>,
) {
    for (entity, sex_opt, tone_opt, hair_opt, clothing_opt, name_opt, faction_opt) in query.iter() {
        let is_female = matches!(sex_opt, Some(BiologicalSex::Female));
        let sex_str = if is_female { "female" } else { "male" };

        let mut entity_cmds = commands.entity(entity);
        entity_cmds.insert((PersonVisual, FacingDirection::South, EntityFogState::default()));
        if clothing_opt.is_none() {
            entity_cmds.insert(ClothingVisual::default());
        }

        if *art_mode == ArtMode::Ascii {
            let img = if is_female {
                textures.person_female_ascii.clone()
            } else {
                textures.person_male_ascii.clone()
            };
            let mut sprite = Sprite::from_image(img);
            sprite.color = Color::WHITE;
            sprite.anchor = Anchor::BottomCenter;
            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    VisualChild,
                    VisualLayer::Body,
                    sprite,
                    Transform::from_xyz(0.0, -8.0, 0.0),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        } else {
            let tone_str = tone_opt.map(|t| t.as_str()).unwrap_or("tan");
            let hair_str = hair_opt.map(|h| h.as_str()).unwrap_or("brown");
            let cloth_str = clothing_opt.map(|c| c.color_key).unwrap_or("tan");

            let body_img = sprite_lib.get(&format!("body_{sex_str}_{tone_str}_s_a"))
                .cloned().unwrap_or_else(|| textures.person_male_ascii.clone());
            let hair_img = sprite_lib.get(&format!("hair_{sex_str}_{hair_str}_s_a"))
                .cloned().unwrap_or_else(|| textures.person_male_ascii.clone());
            let cloth_img = sprite_lib.get(&format!("clothing_{sex_str}_{cloth_str}_s_a"))
                .cloned().unwrap_or_else(|| textures.person_male_ascii.clone());

            let mut body_sprite = Sprite::from_image(body_img);
            body_sprite.color = Color::WHITE;
            body_sprite.anchor = Anchor::BottomCenter;

            let mut hair_sprite = Sprite::from_image(hair_img);
            hair_sprite.anchor = Anchor::BottomCenter;

            let mut cloth_sprite = Sprite::from_image(cloth_img);
            cloth_sprite.color = Color::NONE;
            cloth_sprite.anchor = Anchor::BottomCenter;

            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    VisualChild, VisualLayer::Body,
                    body_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.0),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
                parent.spawn((
                    VisualChild, VisualLayer::Clothing,
                    cloth_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.1),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
                parent.spawn((
                    VisualChild, VisualLayer::Hair,
                    hair_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.2),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        }

        let is_player = faction_opt.map_or(false, |m| m.faction_id == player_faction.faction_id);
        if is_player {
            let label_text = name_opt.map(|n| n.as_str().to_string()).unwrap_or_default();
            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    PersonNameLabel,
                    Text2d::new(label_text),
                    TextFont { font_size: 8.0, ..default() },
                    TextColor(Color::WHITE),
                    TextLayout::default(),
                    // Sprite is 16px tall, bottom at Y=-8 → top at Y=+8; +3px gap
                    Transform::from_xyz(0.0, 11.0, 0.5),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
        }
    }
}

pub fn get_plant_texture(
    textures: &EntityTextures,
    kind: PlantKind,
    stage: GrowthStage,
) -> Handle<Image> {
    match kind {
        PlantKind::Tree => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.tree_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.tree_seedling_ascii.clone(),
            _ => textures.tree_mature_ascii.clone(),
        },
        PlantKind::BerryBush => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.plant_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.plant_seedling_ascii.clone(),
            _ => textures.plant_bush_mature_ascii.clone(),
        },
        _ => match stage {
            GrowthStage::Seed => textures.plant_seed_ascii.clone(),
            GrowthStage::Seedling => textures.plant_seedling_ascii.clone(),
            GrowthStage::Harvested => textures.plant_seedling_ascii.clone(),
            _ => textures.plant_grain_mature_ascii.clone(),
        },
    }
}

pub fn spawn_plant_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Plant), Without<PlantVisual>>,
    textures: Res<EntityTextures>,
) {
    for (entity, plant) in query.iter() {
        let img = get_plant_texture(&textures, plant.kind, plant.stage);
        let mut sprite = Sprite::from_image(img);
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert((PlantVisual, EntityFogState::default()));
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.5),
                GlobalTransform::default(),
                Visibility::Inherited,
                InheritedVisibility::default(),
            ));
        });
    }
}

pub fn update_plant_sprites(
    textures: Res<EntityTextures>,
    query: Query<(&Plant, &Children), (With<PlantVisual>, Changed<Plant>)>,
    mut sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    for (plant, children) in query.iter() {
        let img = get_plant_texture(&textures, plant.kind, plant.stage);
        for &child in children.iter() {
            if let Ok(mut sprite) = sprites.get_mut(child) {
                sprite.image = img.clone();
            }
        }
    }
}

pub fn animate_person_sprites(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut persons: Query<(
        &PersonAI,
        Option<&BiologicalSex>,
        Option<&SkinTone>,
        Option<&HairColor>,
        Option<&ClothingVisual>,
        &Transform,
        &Children,
        &mut FacingDirection,
    ), With<Person>>,
    mut child_sprites: Query<(&mut Sprite, &VisualLayer), With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }

    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;

    for (ai, sex_opt, tone_opt, hair_opt, clothing_opt, transform, children, mut facing) in persons.iter_mut() {
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let diff = target_world - transform.translation.truncate();
        let is_moving = diff.length() > 2.0;

        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 { FacingDirection::East } else { FacingDirection::West }
            } else {
                if diff.y > 0.0 { FacingDirection::North } else { FacingDirection::South }
            };
        }

        let is_female = matches!(sex_opt, Some(BiologicalSex::Female));
        let sex_str = if is_female { "female" } else { "male" };
        let tone_str = tone_opt.map(|t| t.as_str()).unwrap_or("tan");
        let hair_str = hair_opt.map(|h| h.as_str()).unwrap_or("brown");
        let cloth_str = clothing_opt.map(|c| c.color_key).unwrap_or("tan");
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };

        let body_key = format!("body_{sex_str}_{tone_str}_{dir}_{frame_str}");
        let hair_key = format!("hair_{sex_str}_{hair_str}_{dir}_{frame_str}");
        let cloth_key = format!("clothing_{sex_str}_{cloth_str}_{dir}_{frame_str}");

        let clothing_visible = clothing_opt.map(|c| c.visible).unwrap_or(false);
        for &child in children.iter() {
            if let Ok((mut sprite, layer)) = child_sprites.get_mut(child) {
                if *layer == VisualLayer::Clothing {
                    sprite.color = if clothing_visible { Color::WHITE } else { Color::NONE };
                    if !clothing_visible { continue; }
                }
                let key = match layer {
                    VisualLayer::Body => body_key.as_str(),
                    VisualLayer::Hair => hair_key.as_str(),
                    VisualLayer::Clothing => cloth_key.as_str(),
                };
                if let Some(img) = sprite_lib.get(key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn animate_wolves_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut wolves: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Wolf>>,
    mut child_sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;
    for (transform, children, mut facing, mut last_pos) in wolves.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 { FacingDirection::East } else { FacingDirection::West }
            } else {
                if diff.y > 0.0 { FacingDirection::North } else { FacingDirection::South }
            };
        }
        last_pos.0 = pos;
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };
        let key = format!("anim_wolf_{dir}_{frame_str}");
        for &child in children.iter() {
            if let Ok(mut sprite) = child_sprites.get_mut(child) {
                if let Some(img) = sprite_lib.get(&key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

pub fn animate_deer_system(
    time: Res<Time>,
    art_mode: Res<ArtMode>,
    sprite_lib: Res<SpriteLibrary>,
    mut deer: Query<(&Transform, &Children, &mut FacingDirection, &mut LastPos), With<Deer>>,
    mut child_sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    if *art_mode == ArtMode::Ascii {
        return;
    }
    let frame_b = (time.elapsed_secs() * 4.0).floor() as u64 % 2 == 1;
    for (transform, children, mut facing, mut last_pos) in deer.iter_mut() {
        let pos = transform.translation.truncate();
        let diff = pos - last_pos.0;
        let is_moving = diff.length() > 0.5;
        if is_moving {
            *facing = if diff.x.abs() > diff.y.abs() {
                if diff.x > 0.0 { FacingDirection::East } else { FacingDirection::West }
            } else {
                if diff.y > 0.0 { FacingDirection::North } else { FacingDirection::South }
            };
        }
        last_pos.0 = pos;
        let dir = facing.as_str();
        let frame_str = if is_moving && frame_b { "b" } else { "a" };
        let key = format!("anim_deer_{dir}_{frame_str}");
        for &child in children.iter() {
            if let Ok(mut sprite) = child_sprites.get_mut(child) {
                if let Some(img) = sprite_lib.get(&key) {
                    if sprite.image != *img {
                        sprite.image = img.clone();
                    }
                }
            }
        }
    }
}

/// Hide entities that don't belong on the layer the camera is viewing.
/// Surface mode (CameraViewZ == i32::MAX): show entities whose Z equals
/// the surface_z of their tile (i.e. above-ground entities). Underground
/// mode: show only entities whose Z equals camera_view_z.
/// Entities in explored-but-not-visible tiles remain visible but get a dim tint
/// applied by apply_entity_fog_tint_system.
pub fn update_entity_z_visibility_system(
    camera_view_z: Res<crate::rendering::camera::CameraViewZ>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    fog_map: Res<crate::rendering::fog::FogMap>,
    mut q: Query<
        (
            &Transform,
            &mut Visibility,
            &mut EntityFogState,
            Option<&PersonAI>,
            Has<Person>,
        ),
        bevy::prelude::Or<(
            With<Person>,
            With<Wolf>,
            With<Deer>,
            With<Plant>,
            With<Bed>,
            With<Wall>,
            With<FactionCenter>,
            With<Blueprint>,
        )>,
    >,
) {
    use crate::world::terrain::TILE_SIZE;
    let cam_z = camera_view_z.0;
    for (transform, mut vis, mut fog_state, person_ai, is_person) in q.iter_mut() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let surf_z = chunk_map.surface_z_at(tx, ty);
        let entity_z = match person_ai {
            Some(ai) if is_person => ai.current_z as i32,
            _ => surf_z,
        };
        let should_show = if cam_z == i32::MAX {
            entity_z == surf_z
        } else {
            entity_z == cam_z
        };
        let fog_visible = fog_map.is_visible((tx as i16, ty as i16));
        let fog_explored = fog_map.is_explored((tx as i16, ty as i16));
        let new_vis = if should_show && (fog_visible || fog_explored) {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *vis != new_vis {
            *vis = new_vis;
        }
        if should_show {
            let new_fog_state = if fog_visible {
                EntityFogState::Visible
            } else {
                EntityFogState::Explored
            };
            if *fog_state != new_fog_state {
                *fog_state = new_fog_state;
            }
        }
    }
}

/// Apply the correct sprite color to each entity based on fog state and faction membership.
/// Replaces update_faction_sprite_colors by combining fog tinting with faction coloring.
pub fn apply_entity_fog_tint_system(
    entities: Query<
        (&Visibility, &EntityFogState, &Children),
        bevy::prelude::Or<(
            With<Person>,
            With<Wolf>,
            With<Deer>,
            With<Plant>,
            With<Bed>,
            With<Wall>,
            With<FactionCenter>,
            With<Blueprint>,
        )>,
    >,
    mut child_sprites: Query<(&mut Sprite, Option<&AnimalSexTint>), With<VisualChild>>,
) {
    for (vis, fog_state, children) in entities.iter() {
        if *vis == Visibility::Hidden {
            continue;
        }
        let fog_factor = if *fog_state == EntityFogState::Visible {
            1.0f32
        } else {
            0.35
        };
        for &child in children.iter() {
            if let Ok((mut sprite, sex_tint)) = child_sprites.get_mut(child) {
                let alpha = sprite.color.to_srgba().alpha;
                let base = sex_tint
                    .map(|t| t.0.to_srgba())
                    .unwrap_or(bevy::color::Srgba::WHITE);
                let new_color = Color::srgba(
                    base.red * fog_factor,
                    base.green * fog_factor,
                    base.blue * fog_factor,
                    alpha,
                );
                if sprite.color != new_color {
                    sprite.color = new_color;
                }
            }
        }
    }
}

pub fn toggle_art_mode(keyboard: Res<ButtonInput<KeyCode>>, mut art_mode: ResMut<ArtMode>) {
    if keyboard.just_pressed(KeyCode::KeyT) {
        *art_mode = match *art_mode {
            ArtMode::Ascii => ArtMode::Pixel,
            ArtMode::Pixel => ArtMode::Ascii,
        };
        info!("Art Mode changed to: {:?}", *art_mode);
    }
}
pub fn handle_art_mode_change(
    mut commands: Commands,
    art_mode: Res<ArtMode>,
    people: Query<Entity, With<PersonVisual>>,
    wolves: Query<Entity, With<WolfVisual>>,
    deer: Query<Entity, With<DeerVisual>>,
    walls: Query<Entity, With<WallVisual>>,
    beds: Query<Entity, With<BedVisual>>,
    centers: Query<Entity, With<FactionCenterVisual>>,
    plants: Query<Entity, With<PlantVisual>>,
    blueprints: Query<Entity, With<BlueprintVisual>>,
    children: Query<(Entity, &Children)>,
) {
    if art_mode.is_changed() && !art_mode.is_added() {
        let all_visuals = people
            .iter()
            .chain(wolves.iter())
            .chain(deer.iter())
            .chain(walls.iter())
            .chain(beds.iter())
            .chain(centers.iter())
            .chain(plants.iter())
            .chain(blueprints.iter());

        for entity in all_visuals {
            if let Ok((_, children)) = children.get(entity) {
                for &child in children.iter() {
                    // Only despawn if it's a visual child to avoid destroying actual game logic children if any
                    commands.entity(child).despawn_recursive();
                }
            }
            commands.entity(entity).remove::<(
                PersonVisual,
                WolfVisual,
                DeerVisual,
                WallVisual,
                BedVisual,
                FactionCenterVisual,
                PlantVisual,
                BlueprintVisual,
            )>();
        }
    }
}


/// Updates ClothingVisual color key whenever a person's Equipment changes.
/// The animate system picks up the new key on the next frame automatically.
pub fn update_clothing_from_equipment(
    mut persons: Query<(&Equipment, Option<&mut ClothingVisual>), (With<Person>, Changed<Equipment>)>,
) {
    for (equip, clothing_opt) in &mut persons {
        if let Some(mut clothing) = clothing_opt {
            let has_armor = equip.items.contains_key(&EquipmentSlot::TorsoArmor);
            clothing.visible = has_armor;
            clothing.color_key = if has_armor { "grey" } else { "tan" };
        }
    }
}

pub fn spawn_blueprint_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Blueprint), (With<Blueprint>, Without<BlueprintVisual>)>,
    textures: Res<EntityTextures>,
) {
    for (entity, bp) in query.iter() {
        let scaffold_img = textures.blueprint_ascii.clone();

        let mut scaffold_sprite = Sprite::from_image(scaffold_img);
        scaffold_sprite.anchor = Anchor::BottomCenter;

        let ghost_img = match bp.kind {
            BuildSiteKind::Wall => textures.wall_ascii.clone(),
            BuildSiteKind::Bed => textures.bed_ascii.clone(),
        };

        let mut ghost_sprite = Sprite::from_image(ghost_img);
        ghost_sprite.anchor = Anchor::BottomCenter;
        ghost_sprite.color = Color::srgba(1.0, 1.0, 1.0, 0.4);

        commands
            .entity(entity)
            .insert((BlueprintVisual, EntityFogState::default()))
            .with_children(|parent| {
                parent.spawn((
                    VisualChild,
                    scaffold_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.2),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));

                parent.spawn((
                    VisualChild,
                    ghost_sprite,
                    Transform::from_xyz(0.0, -8.0, 0.1),
                    GlobalTransform::default(),
                    Visibility::Inherited,
                    InheritedVisibility::default(),
                ));
            });
    }
}
