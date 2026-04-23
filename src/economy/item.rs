use bevy::prelude::*;
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
            ItemMaterial::Wood    => 1.0,
            ItemMaterial::Stone   => 1.5,
            ItemMaterial::Iron    => 3.0,
            ItemMaterial::Steel   => 6.0,
            ItemMaterial::Leather => 2.5,
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
            ItemQuality::Poor       => 0.5,
            ItemQuality::Normal     => 1.0,
            ItemQuality::Fine       => 2.0,
            ItemQuality::Masterwork => 5.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub good:     Good,
    pub material: Option<ItemMaterial>,
    pub quality:  Option<ItemQuality>,
}

impl Item {
    pub fn new_commodity(good: Good) -> Self {
        Self { good, material: None, quality: None }
    }

    pub fn new_manufactured(good: Good, material: ItemMaterial, quality: ItemQuality) -> Self {
        Self { good, material: Some(material), quality: Some(quality) }
    }

    pub fn is_manufactured(&self) -> bool {
        self.material.is_some() || self.quality.is_some()
    }

    pub fn multiplier(&self) -> f32 {
        let mat_m = self.material.map(|m| m.multiplier()).unwrap_or(1.0);
        let qual_m = self.quality.map(|q| q.multiplier()).unwrap_or(1.0);
        mat_m * qual_m
    }
}
