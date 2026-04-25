use bevy::prelude::*;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::construction::{Bed, Blueprint, Wall, BuildSiteKind};
use crate::simulation::faction::{FactionMember, PlayerFaction, FactionCenter, PlayerFactionMarker};
use crate::simulation::person::Person;
use crate::simulation::plants::{Plant, PlantKind, GrowthStage};
use crate::simulation::reproduction::BiologicalSex;
use crate::rendering::pixel_art::EntityTextures;

use bevy::sprite::Anchor;

/// Note: All entity sprites in this game follow a unified alignment rule:
/// 1. Logical entities are spawned at the mathematical center of their tile (wx, wy).
/// 2. Sprites are attached as children with `Anchor::BottomCenter`.
/// 3. To align the sprite's bottom edge with the tile's visual bottom,
///    a universal Y-offset of -8.0 is applied to the child transform.
/// This ensures that 16px tall walls and 36px tall people both stand on the same floor.

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

#[derive(Component)]
pub struct FactionCenterVisual;

#[derive(Component)]
pub struct PlantVisual;

#[derive(Component)]
pub struct BlueprintVisual;

#[derive(Component)]
pub struct VisualChild;

/// Helper to spawn a visual child with the correct anchor and alignment offset.
pub fn spawn_visual_child(commands: &mut Commands, entity: Entity, mut sprite: Sprite, z_offset: f32) {
    sprite.anchor = Anchor::BottomCenter;
    commands.entity(entity).with_children(|parent| {
        parent.spawn((
            VisualChild,
            sprite,
            Transform::from_xyz(0.0, -8.0, z_offset),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ));
    });
}

pub fn spawn_bed_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Bed>, Without<BedVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.bed.clone());
        sprite.custom_size = Some(Vec2::new(16.0, 10.0));
        
        commands.entity(entity).insert(BedVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn spawn_wall_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Wall>, Without<WallVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.wall.clone());
        sprite.custom_size = Some(Vec2::new(16.0, 16.0));

        commands.entity(entity).insert(WallVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn spawn_faction_center_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&PlayerFactionMarker>), (With<FactionCenter>, Without<FactionCenterVisual>)>,
    textures: Res<EntityTextures>,
) {
    for (entity, player_marker) in query.iter() {
        let mut sprite = Sprite::from_image(textures.camp.clone());
        sprite.custom_size = Some(Vec2::new(24.0, 24.0));
        
        if player_marker.is_some() {
            sprite.color = Color::srgb(0.55, 0.85, 1.0);
        }

        commands.entity(entity).insert(FactionCenterVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn spawn_wolf_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Wolf>, Without<WolfVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.wolf.clone());
        sprite.custom_size = Some(Vec2::new(24.0, 36.0));

        commands.entity(entity).insert(WolfVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn spawn_deer_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Deer>, Without<DeerVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.deer.clone());
        sprite.custom_size = Some(Vec2::new(24.0, 36.0));

        commands.entity(entity).insert(DeerVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn spawn_person_sprites(
    mut commands: Commands,
    query: Query<(Entity, Option<&BiologicalSex>), (With<Person>, Without<PersonVisual>)>,
    textures: Res<EntityTextures>,
) {
    for (entity, sex_opt) in query.iter() {
        let mut sprite = Sprite::default();
        sprite.image = match sex_opt {
            Some(BiologicalSex::Female) => textures.person_female.clone(),
            _ => textures.person_male.clone(),
        };
        sprite.color = Color::WHITE;
        sprite.custom_size = Some(Vec2::new(24.0, 36.0));

        commands.entity(entity).insert(PersonVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.1);
    }
}

pub fn get_plant_texture(textures: &EntityTextures, kind: PlantKind, stage: GrowthStage) -> Handle<Image> {
    match kind {
        PlantKind::Tree => match stage {
            GrowthStage::Seed     => textures.plant_seed.clone(),
            GrowthStage::Seedling => textures.tree_seedling.clone(),
            _ => textures.tree_mature.clone(),
        },
        _ => match stage {
            GrowthStage::Seed => textures.plant_seed.clone(),
            GrowthStage::Seedling => textures.plant_seedling.clone(),
            _ => textures.plant_mature.clone(),
        }
    }
}

pub fn spawn_plant_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Plant), Without<PlantVisual>>,
    textures: Res<EntityTextures>,
) {
    for (entity, plant) in query.iter() {
        let mut sprite = Sprite::from_image(get_plant_texture(&textures, plant.kind, plant.stage));
        sprite.custom_size = Some(Vec2::new(24.0, 36.0));

        commands.entity(entity).insert(PlantVisual);
        spawn_visual_child(&mut commands, entity, sprite, 0.5);
    }
}

pub fn update_plant_sprites(
    textures: Res<EntityTextures>,
    query: Query<(&Plant, &Children), (With<PlantVisual>, Changed<Plant>)>,
    mut sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    for (plant, children) in query.iter() {
        let texture = get_plant_texture(&textures, plant.kind, plant.stage);
        for &child in children.iter() {
            if let Ok(mut sprite) = sprites.get_mut(child) {
                sprite.image = texture.clone();
            }
        }
    }
}

pub fn update_faction_sprite_colors(
    player_faction: Res<PlayerFaction>,
    persons: Query<
        (&FactionMember, &Children),
        (With<Person>, With<PersonVisual>, Changed<FactionMember>),
    >,
    mut sprites: Query<&mut Sprite, With<VisualChild>>,
) {
    for (member, children) in persons.iter() {
        let color = if member.faction_id == player_faction.faction_id {
            Color::srgb(0.55, 0.85, 1.0)
        } else {
            Color::WHITE
        };
        for &child in children.iter() {
            if let Ok(mut sprite) = sprites.get_mut(child) {
                sprite.color = color;
            }
        }
    }
}

pub fn spawn_blueprint_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Blueprint), (With<Blueprint>, Without<BlueprintVisual>)>,
    textures: Res<EntityTextures>,
) {
    for (entity, bp) in query.iter() {
        let mut scaffold_sprite = Sprite::from_image(textures.blueprint.clone());
        scaffold_sprite.custom_size = Some(Vec2::new(16.0, 16.0));
        scaffold_sprite.anchor = Anchor::BottomCenter;

        let ghost_image = match bp.kind {
            BuildSiteKind::Wall => textures.wall.clone(),
            BuildSiteKind::Bed  => textures.bed.clone(),
        };
        let mut ghost_sprite = Sprite::from_image(ghost_image);
        ghost_sprite.custom_size = match bp.kind {
            BuildSiteKind::Wall => Some(Vec2::new(16.0, 16.0)),
            BuildSiteKind::Bed  => Some(Vec2::new(16.0, 10.0)),
        };
        ghost_sprite.anchor = Anchor::BottomCenter;
        ghost_sprite.color = Color::srgba(1.0, 1.0, 1.0, 0.4);

        commands.entity(entity).insert(BlueprintVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                scaffold_sprite,
                Transform::from_xyz(0.0, -8.0, 0.2),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
            parent.spawn((
                VisualChild,
                ghost_sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        });
    }
}
