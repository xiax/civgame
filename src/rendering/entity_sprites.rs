use bevy::prelude::*;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::construction::{Bed, Blueprint, Wall, BuildSiteKind};
use crate::simulation::faction::{FactionMember, PlayerFaction, FactionCenter, PlayerFactionMarker};
use crate::simulation::person::Person;
use crate::simulation::reproduction::BiologicalSex;
use crate::rendering::pixel_art::EntityTextures;

use bevy::sprite::Anchor;

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
pub struct VisualChild;

pub fn spawn_bed_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Bed>, Without<BedVisual>)>,
    textures: Res<EntityTextures>,
) {
    for entity in query.iter() {
        let mut sprite = Sprite::from_image(textures.bed.clone());
        sprite.custom_size = Some(Vec2::new(16.0, 10.0));
        sprite.anchor = Anchor::Center;

        commands.entity(entity).insert(BedVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, 0.0, 0.1),
                Visibility::Visible,
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
        let mut sprite = Sprite::from_image(textures.wall.clone());
        sprite.custom_size = Some(Vec2::new(16.0, 16.0));
        sprite.anchor = Anchor::Center;

        commands.entity(entity).insert(WallVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, 0.0, 0.1),
                Visibility::Visible,
            ));
        });
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
        sprite.anchor = Anchor::Center;
        
        // Tint blue if it's the player's faction
        if player_marker.is_some() {
            sprite.color = Color::srgb(0.55, 0.85, 1.0);
        }

        commands.entity(entity).insert(FactionCenterVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, 0.0, 0.1),
                Visibility::Visible,
            ));
        });
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
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert(WolfVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                Visibility::Visible,
            ));
        });
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
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert(DeerVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                Visibility::Visible,
            ));
        });
    }
}

/// Tint sprites blue for the player's faction, white for everyone else.
/// Uses Changed<FactionMember> so it only runs when membership actually changes.
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

#[derive(Component)]
pub struct BlueprintVisual;

/// Spawn visual scaffold sprites for newly placed Blueprint entities.
pub fn spawn_blueprint_sprites(
    mut commands: Commands,
    query: Query<(Entity, &Blueprint), (With<Blueprint>, Without<BlueprintVisual>)>,
    textures: Res<EntityTextures>,
) {
    for (entity, bp) in query.iter() {
        // Blueprint background (scaffold)
        let mut scaffold_sprite = Sprite::from_image(textures.blueprint.clone());
        scaffold_sprite.custom_size = Some(Vec2::new(16.0, 16.0));
        scaffold_sprite.anchor = Anchor::Center;

        // "Ghost" of the finished product
        let ghost_image = match bp.kind {
            BuildSiteKind::Wall => textures.wall.clone(),
            BuildSiteKind::Bed  => textures.bed.clone(),
        };
        let mut ghost_sprite = Sprite::from_image(ghost_image);
        ghost_sprite.custom_size = match bp.kind {
            BuildSiteKind::Wall => Some(Vec2::new(16.0, 16.0)),
            BuildSiteKind::Bed  => Some(Vec2::new(16.0, 10.0)),
        };
        ghost_sprite.anchor = Anchor::Center;
        ghost_sprite.color = Color::srgba(1.0, 1.0, 1.0, 0.4);

        commands.entity(entity).insert(BlueprintVisual).with_children(|parent| {
            // Scaffold on top
            parent.spawn((
                VisualChild,
                scaffold_sprite,
                Transform::from_xyz(0.0, 0.0, 0.2),
                Visibility::Visible,
            ));
            // Ghost underneath
            parent.spawn((
                VisualChild,
                ghost_sprite,
                Transform::from_xyz(0.0, 0.0, 0.1),
                Visibility::Visible,
            ));
        });
    }
}

/// Spawn visual sprites for newly added Person entities.
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
        sprite.anchor = Anchor::BottomCenter;

        commands.entity(entity).insert(PersonVisual).with_children(|parent| {
            parent.spawn((
                VisualChild,
                sprite,
                Transform::from_xyz(0.0, -8.0, 0.1),
                Visibility::Visible,
            ));
        });
    }
}
