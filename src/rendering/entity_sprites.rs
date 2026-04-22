use bevy::prelude::*;
use crate::simulation::animals::{Deer, Wolf};
use crate::simulation::person::Person;
use crate::simulation::mood::Mood;

#[derive(Component)]
pub struct PersonSprite;

pub fn spawn_wolf_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Wolf>, Without<Mesh2d>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let mesh  = meshes.add(Rectangle::new(8.0, 8.0));
    let color = materials.add(ColorMaterial::from_color(Color::srgb(0.45, 0.25, 0.1)));
    for entity in query.iter() {
        commands.entity(entity).insert((
            Mesh2d(mesh.clone()),
            MeshMaterial2d(color.clone()),
            Visibility::Visible,
        ));
    }
}

pub fn spawn_deer_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Deer>, Without<Mesh2d>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let mesh  = meshes.add(Rectangle::new(6.0, 6.0));
    let color = materials.add(ColorMaterial::from_color(Color::srgb(0.8, 0.65, 0.35)));
    for entity in query.iter() {
        commands.entity(entity).insert((
            Mesh2d(mesh.clone()),
            MeshMaterial2d(color.clone()),
            Visibility::Visible,
        ));
    }
}

/// Sync person entity sprite color with their mood.
pub fn entity_sprite_sync(
    mut query: Query<(&Mood, &mut MeshMaterial2d<ColorMaterial>), With<Person>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    for (mood, material_handle) in query.iter_mut() {
        let t = (mood.0 as f32 + 128.0) / 255.0; // 0..1
        // Interpolate: red (despairing) → cyan (happy)
        let color = Color::srgb(
            1.0 - t * 0.8,
            0.3 + t * 0.7,
            t,
        );
        if let Some(mat) = materials.get_mut(material_handle.id()) {
            mat.color = color;
        }
    }
}

/// Spawn visual sprites for newly added Person entities.
pub fn spawn_person_sprites(
    mut commands: Commands,
    query: Query<Entity, (With<Person>, Without<Mesh2d>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let person_mesh = meshes.add(Rectangle::new(6.0, 6.0));
    let default_color = materials.add(ColorMaterial::from_color(Color::srgb(0.0, 0.9, 0.9)));

    for entity in query.iter() {
        commands.entity(entity).insert((
            Mesh2d(person_mesh.clone()),
            MeshMaterial2d(default_color.clone()),
            Visibility::Visible,
        ));
    }
}
