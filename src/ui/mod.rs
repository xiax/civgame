use bevy::prelude::*;
use bevy_egui::EguiPlugin;

pub mod selection;
pub mod inspector;
pub mod economy_panel;
pub mod hud;
pub mod world_map;

pub use selection::SelectedEntity;

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin)
            .insert_resource(SelectedEntity::default())
            .insert_resource(world_map::WorldMapOpen::default())
            .insert_resource(world_map::WorldMapTexture::default())
            .add_systems(
                Update,
                (
                    world_map::world_map_toggle_system,
                    selection::click_to_select_system,
                    inspector::inspector_panel_system,
                    economy_panel::economy_panel_system,
                    hud::hud_system,
                    world_map::world_map_system,
                ),
            );
    }
}
