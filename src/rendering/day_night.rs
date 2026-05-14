use crate::world::seasons::Calendar;
use bevy::prelude::*;

#[derive(Component)]
pub struct DayNightOverlay;

/// Day-night overlay's render-z. Pinned at 90.0 so it sits above every
/// world sprite (terrain ~0, ground items ~0.3, walls ~0.4, entities ~0.5)
/// even after the tilted-view projection bakes a depth offset onto each
/// anchored entity. Verified by `projection::tests::tilted_dz_below_day_night`.
pub const OVERLAY_Z: f32 = 90.0;
const OVERLAY_HALF_SIZE: f32 = 1_000_000.0;

pub fn spawn_day_night_overlay(mut commands: Commands) {
    commands.spawn((
        DayNightOverlay,
        Sprite {
            color: Color::srgba(0.0, 0.0, 0.0, 0.0),
            custom_size: Some(Vec2::splat(OVERLAY_HALF_SIZE * 2.0)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, OVERLAY_Z),
        GlobalTransform::default(),
        Visibility::Visible,
        InheritedVisibility::default(),
    ));
}

pub fn update_day_night_overlay_system(
    calendar: Res<Calendar>,
    camera_q: Query<&Transform, (With<Camera>, Without<DayNightOverlay>)>,
    mut overlay_q: Query<(&mut Sprite, &mut Transform), With<DayNightOverlay>>,
) {
    let Ok((mut sprite, mut transform)) = overlay_q.get_single_mut() else {
        return;
    };

    if let Ok(cam) = camera_q.get_single() {
        transform.translation.x = cam.translation.x;
        transform.translation.y = cam.translation.y;
        transform.translation.z = OVERLAY_Z;
    }

    let (r, g, b, a) = tint_for_fraction(calendar.day_fraction());
    sprite.color = Color::srgba(r, g, b, a);
}

/// Piecewise interpolation from `day_fraction()` to an RGBA overlay tint.
/// Returns straight (non-premultiplied) sRGB components.
fn tint_for_fraction(frac: f32) -> (f32, f32, f32, f32) {
    // Anchor colors.
    let amber_dusk = (1.00, 0.55, 0.35);
    let indigo_night = (0.10, 0.15, 0.35);
    let amber_dawn = (1.00, 0.65, 0.40);
    let amber_predusk = (1.00, 0.75, 0.50);

    let night_alpha = 0.55;
    let predusk_alpha = 0.15;

    if frac < 0.05 {
        // Dawn 0.00 → 0.05: indigo night fading to clear amber dawn.
        let t = frac / 0.05;
        let (r, g, b) = lerp_rgb(indigo_night, amber_dawn, t);
        let a = lerp(night_alpha, 0.0, t);
        (r, g, b, a)
    } else if frac < 0.55 {
        // Day: clear.
        (1.0, 1.0, 1.0, 0.0)
    } else if frac < 0.65 {
        // Pre-dusk warming: 0 → 0.15 alpha, warm amber.
        let t = (frac - 0.55) / 0.10;
        let (r, g, b) = amber_predusk;
        let a = lerp(0.0, predusk_alpha, t);
        (r, g, b, a)
    } else if frac < 0.85 {
        // Dusk: amber → indigo, 0.15 → 0.55 alpha.
        let t = (frac - 0.65) / 0.20;
        let (r, g, b) = lerp_rgb(amber_dusk, indigo_night, t);
        let a = lerp(predusk_alpha, night_alpha, t);
        (r, g, b, a)
    } else {
        // Night: deep indigo, full alpha.
        let (r, g, b) = indigo_night;
        (r, g, b, night_alpha)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn lerp_rgb(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    (lerp(a.0, b.0, t), lerp(a.1, b.1, t), lerp(a.2, b.2, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_is_clear() {
        let (_, _, _, a) = tint_for_fraction(0.3);
        assert!(a.abs() < 1e-6);
    }

    #[test]
    fn night_is_dark() {
        let (_, _, _, a) = tint_for_fraction(0.92);
        assert!(a > 0.5);
    }

    #[test]
    fn dusk_ramps() {
        let (_, _, _, a_early) = tint_for_fraction(0.66);
        let (_, _, _, a_late) = tint_for_fraction(0.84);
        assert!(a_early < a_late);
    }

    #[test]
    fn dawn_fades_to_clear() {
        let (_, _, _, a_early) = tint_for_fraction(0.005);
        let (_, _, _, a_late) = tint_for_fraction(0.049);
        assert!(a_early > a_late);
    }
}
