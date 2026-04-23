use bevy::prelude::*;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::person::Person;
use crate::simulation::mood::Mood;

#[derive(Component)]
pub struct PersonSprite;

pub fn spawn_wolf_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Wolf>, Without<Sprite>)>,
) {
    let color = Color::srgb(0.45, 0.25, 0.1);
    for entity in query.iter() {
        commands.entity(entity).insert((
            Sprite::from_color(color, Vec2::new(8.0, 8.0)),
            Visibility::Visible,
        ));
    }
}

pub fn spawn_deer_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Deer>, Without<Sprite>)>,
) {
    let color = Color::srgb(0.8, 0.65, 0.35);
    for entity in query.iter() {
        commands.entity(entity).insert((
            Sprite::from_color(color, Vec2::new(6.0, 6.0)),
            Visibility::Visible,
        ));
    }
}

/// Sync person entity sprite color with their mood.
pub fn entity_sprite_sync(
    mut query: Query<(&Mood, &mut Sprite), With<Person>>,
) {
    for (mood, mut sprite) in query.iter_mut() {
        let t = (mood.0 as f32 + 128.0) / 255.0; // 0..1
        // Interpolate: red (despairing) → cyan (happy)
        sprite.color = Color::srgb(
            1.0 - t * 0.8,
            0.3 + t * 0.7,
            t,
        );
    }
}

/// Spawn visual sprites for newly added Person entities.
pub fn spawn_person_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Person>, Without<Sprite>)>,
) {
    let default_color = Color::srgb(0.0, 0.9, 0.9);

    for entity in query.iter() {
        commands.entity(entity).insert((
            Sprite::from_color(default_color, Vec2::new(6.0, 6.0)),
            Visibility::Visible,
        ));
    }
}
