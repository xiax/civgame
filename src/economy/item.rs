use super::goods::Good;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemMaterial {
    Wood,
    Stone,
    Iron,
    Steel,
    Leather,
}

impl ItemMaterial {
    pub fn multiplier(&self) -> f32 {
        match self {
            ItemMaterial::Wood => 1.0,
            ItemMaterial::Stone => 1.5,
            ItemMaterial::Iron => 3.0,
            ItemMaterial::Steel => 6.0,
            ItemMaterial::Leather => 2.5,
        }
    }

    pub fn weight_multiplier(&self) -> f32 {
        match self {
            ItemMaterial::Wood => 0.7,
            ItemMaterial::Stone => 1.4,
            ItemMaterial::Iron => 1.6,
            ItemMaterial::Steel => 1.5,
            ItemMaterial::Leather => 0.8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemQuality {
    Poor,
    Normal,
    Fine,
    Masterwork,
}

impl ItemQuality {
    pub fn multiplier(&self) -> f32 {
        match self {
            ItemQuality::Poor => 0.5,
            ItemQuality::Normal => 1.0,
            ItemQuality::Fine => 2.0,
            ItemQuality::Masterwork => 5.0,
        }
    }
}

/// Combat damage bonus carried by a wieldable Item (Weapon).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeaponStats {
    pub damage_bonus: u8,
    /// Encoded as hundredths so `Item` stays `Copy + Eq` (1.00 = 100, 1.25 = 125).
    pub attack_speed_pct: u16,
}

impl WeaponStats {
    pub fn attack_speed(self) -> f32 {
        self.attack_speed_pct as f32 / 100.0
    }
}

/// Armor coverage bitset. `Item` must stay `Copy + Eq`, so coverage is a tiny
/// bitmask over four logical groups (head/torso/arms/legs) — combat maps each
/// `BodyPart` hit to the matching bit via `body_part_to_coverage_bit`.
pub mod armor_coverage {
    pub const HEAD: u8 = 1 << 0;
    pub const TORSO: u8 = 1 << 1;
    pub const ARMS: u8 = 1 << 2;
    pub const LEGS: u8 = 1 << 3;
}

/// Damage reduction carried by a wearable Item (Armor / Shield).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArmorStats {
    pub damage_reduction: u8,
    /// 0..100 chance the armor deflects a hit on a covered part.
    pub coverage_pct: u8,
    /// Bitset over `armor_coverage::*` flags.
    pub covered_parts: u8,
}

impl ArmorStats {
    pub fn covers(self, part_bit: u8) -> bool {
        self.covered_parts & part_bit != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub good: Good,
    pub material: Option<ItemMaterial>,
    pub quality: Option<ItemQuality>,
    pub display_name: Option<&'static str>,
    pub weapon_stats: Option<WeaponStats>,
    pub armor_stats: Option<ArmorStats>,
}

impl Item {
    pub fn new_commodity(good: Good) -> Self {
        Self {
            good,
            material: None,
            quality: None,
            display_name: None,
            weapon_stats: None,
            armor_stats: None,
        }
    }

    pub fn new_manufactured(good: Good, material: ItemMaterial, quality: ItemQuality) -> Self {
        let (weapon_stats, armor_stats) = compute_combat_stats(good, material, quality);
        Self {
            good,
            material: Some(material),
            quality: Some(quality),
            display_name: None,
            weapon_stats,
            armor_stats,
        }
    }

    /// Human-readable label for UI display. Uses `display_name` when set
    /// (crafted items carry the recipe name), otherwise falls back to
    /// "Material Good" from `material` + `good.name()`. Quality is appended
    /// in parentheses when present.
    pub fn label(&self) -> String {
        let base = if let Some(dn) = self.display_name {
            dn.to_string()
        } else {
            let mut s = self.good.name().to_string();
            if let Some(mat) = self.material {
                s = format!("{:?} {}", mat, s);
            }
            s
        };
        if let Some(qual) = self.quality {
            format!("{} ({:?})", base, qual)
        } else {
            base
        }
    }

    pub fn is_manufactured(&self) -> bool {
        self.material.is_some() || self.quality.is_some()
    }

    pub fn multiplier(&self) -> f32 {
        let mat_m = self.material.map(|m| m.multiplier()).unwrap_or(1.0);
        let qual_m = self.quality.map(|q| q.multiplier()).unwrap_or(1.0);
        mat_m * qual_m
    }

    /// Weight of one unit, in grams. Material nudges base weight (iron > wood).
    pub fn unit_weight_g(&self) -> u32 {
        let base = self.good.unit_weight_g() as f32;
        let mult = self.material.map(|m| m.weight_multiplier()).unwrap_or(1.0);
        (base * mult).round() as u32
    }

    /// Weight of `qty` units, in grams.
    pub fn stack_weight_g(&self, qty: u32) -> u32 {
        self.unit_weight_g().saturating_mul(qty)
    }
}

/// Derive combat stats from `(good, material, quality)`. Weapons get a
/// `damage_bonus` proportional to material × quality; Shields and Armor get
/// `ArmorStats` covering the body parts they protect. Other goods carry no
/// combat stats (`(None, None)`).
fn compute_combat_stats(
    good: Good,
    material: ItemMaterial,
    quality: ItemQuality,
) -> (Option<WeaponStats>, Option<ArmorStats>) {
    let m = material.multiplier();
    let q = quality.multiplier();
    match good {
        Good::Weapon => {
            let damage_bonus = (2.0 * m * q).round().clamp(1.0, 60.0) as u8;
            // Fine/Masterwork weapons swing slightly faster.
            let attack_speed_pct = match quality {
                ItemQuality::Poor => 90,
                ItemQuality::Normal => 100,
                ItemQuality::Fine => 110,
                ItemQuality::Masterwork => 125,
            };
            (
                Some(WeaponStats {
                    damage_bonus,
                    attack_speed_pct,
                }),
                None,
            )
        }
        Good::Shield => {
            let damage_reduction = (1.0 * m * q).round().clamp(1.0, 30.0) as u8;
            let coverage_pct = 60u8;
            let covered_parts = armor_coverage::TORSO | armor_coverage::ARMS;
            (
                None,
                Some(ArmorStats {
                    damage_reduction,
                    coverage_pct,
                    covered_parts,
                }),
            )
        }
        Good::Armor => {
            let damage_reduction = (2.0 * m * q).round().clamp(1.0, 40.0) as u8;
            let coverage_pct = 80u8;
            // Body armor covers torso + arms; helmets/leggings would be future
            // recipes with their own covered_parts mask.
            let covered_parts = armor_coverage::TORSO | armor_coverage::ARMS;
            (
                None,
                Some(ArmorStats {
                    damage_reduction,
                    coverage_pct,
                    covered_parts,
                }),
            )
        }
        _ => (None, None),
    }
}

