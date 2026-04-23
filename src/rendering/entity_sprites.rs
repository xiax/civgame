use bevy::prelude::*;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::person::Person;
use crate::simulation::reproduction::BiologicalSex;
use crate::rendering::pixel_art::EntityTextures;

use bevy::sprite::Anchor;

#[derive(Component)]
pub struct WolfVisual;

#[derive(Component)]
pub struct DeerVisual;

#[derive(Component)]
pub struct PersonVisual;

#[derive(Component)]
pub struct VisualChild;

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
