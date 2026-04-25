use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::render_asset::RenderAssetUsages;

#[derive(Resource)]
pub struct EntityTextures {
    pub wolf: Handle<Image>,
    pub deer: Handle<Image>,
    pub person_male: Handle<Image>,
    pub person_female: Handle<Image>,
    pub plant_seed: Handle<Image>,
    pub plant_seedling: Handle<Image>,
    pub plant_mature: Handle<Image>,
    pub tree_seedling: Handle<Image>,
    pub tree_mature: Handle<Image>,
    pub camp: Handle<Image>,
    pub bed: Handle<Image>,
    pub blueprint: Handle<Image>,
    pub wall: Handle<Image>,
}

#[derive(Clone, Copy)]
pub struct PixelColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl PixelColor {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

pub fn ascii_to_image(ascii: &[&str], colors: &[(char, PixelColor)]) -> Image {
    let height = ascii.len();
    let width = ascii[0].len();
    let mut data = vec![0; width * height * 4];

    let transparent = PixelColor::new(0, 0, 0, 0);

    for (y, row) in ascii.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            let color = colors
                .iter()
                .find(|(c, _)| *c == ch)
                .map(|(_, col)| col)
                .unwrap_or(&transparent);
            
            let i = (y * width + x) * 4;
            data[i] = color.r;
            data[i + 1] = color.g;
            data[i + 2] = color.b;
            data[i + 3] = color.a;
        }
    }

    Image::new(
        Extent3d {
            width: width as u32,
            height: height as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::all(),
    )
}

pub fn setup_pixel_art(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
) {
    let _t = PixelColor::new(0, 0, 0, 0); // Transparent
    
    // Grays (Wolf/Metal)
    let g = PixelColor::new(140, 140, 145, 255); // Gray
    let d = PixelColor::new(80, 80, 85, 255);   // Dark Gray
    let l_g = PixelColor::new(190, 190, 195, 255); // Light Gray

    // Browns (Deer/Wood/Dirt)
    let b = PixelColor::new(120, 80, 40, 255);  // Brown
    let d_b = PixelColor::new(70, 45, 20, 255);  // Dark Brown
    let l_b = PixelColor::new(160, 110, 60, 255); // Light Brown
    let a = PixelColor::new(220, 200, 170, 255); // Tan/Antler

    // Skin/Hair
    let s = PixelColor::new(255, 215, 190, 255); // Skin
    let s_d = PixelColor::new(230, 170, 140, 255); // Skin Shadow
    let h = PixelColor::new(85, 60, 40, 255);    // Hair
    let h_l = PixelColor::new(120, 95, 60, 255);  // Hair Highlight

    // Nature
    let e = PixelColor::new(50, 160, 50, 255);   // Green (Plant)
    let e_d = PixelColor::new(30, 100, 30, 255);   // Dark Green
    let e_l = PixelColor::new(120, 220, 80, 255); // Light Green
    let r = PixelColor::new(230, 30, 30, 255);   // Red (Fruit)
    let r_l = PixelColor::new(255, 90, 90, 255); // Light Red

    // Basics
    let w = PixelColor::new(255, 255, 255, 255); // White
    let y = PixelColor::new(255, 240, 40, 255);  // Yellow (Eyes/Gold)
    let x = PixelColor::new(20, 20, 20, 255);    // Black/Darkest

    // Wolf: 16x16 (Distinct snout, ears, and stalking posture)
    let wolf_ascii = &[
        "................",
        "................",
        ".......d...d....",
        ".......dg.gd....",
        ".......dgggd....",
        ".......dgygd.dd.",
        "....ddddggggdgd.",
        "...dggggggggggd.",
        "..dgggggggggggd.",
        ".dlgggggggggggd.",
        "dlggggggggggggd.",
        "dggggggggggggd..",
        "dggggd..dggggd..",
        "dgggd....dgggd..",
        "dddd......dddd..",
        "xxxx......xxxx..",
    ];
    let wolf_img = ascii_to_image(wolf_ascii, &[
        ('g', g), ('d', d), ('l', l_g), ('y', y), ('w', w), ('x', x), ('.', _t)
    ]);

    // Deer: 16x16 (Graceful neck and branching antlers)
    let deer_ascii = &[
        "................",
        "...a.a....a.a...",
        "...aaaa..aaaa...",
        "....aa.aa.aa....",
        ".....aaaaaa.....",
        "......abbb......",
        "......abxb......",
        "......abbbbbbb..",
        ".....bbbbbbbbbb.",
        "....lbbbbbbbbbb.",
        "...llbbbbbbbbbb.",
        "..lllbbbbbbbbbb.",
        "..ddb......ddb..",
        "..dd........dd..",
        "..dd........dd..",
        "..xx........xx..",
    ];
    let deer_img = ascii_to_image(deer_ascii, &[
        ('l', l_b), ('b', b), ('d', d_b), ('a', a), ('x', x), ('w', w), ('h', l_b), ('.', _t)
    ]);

    // Male: 16x16 (Caveman - wild hair, fur loincloth, bare chest)
    let male_ascii = &[
        "................",
        "................",
        "....h..hh..h....",
        "...hhhhhhhhhh...",
        "..hhhhsssshhhh..",
        "..hhhswsswshhh..",
        "..hhhhsxsxshhh..",
        "..hhhhhssshhhh..",
        "...ssshhhhhss...",
        "...sbbbbbbbbs...",
        "..ssbbbbbbbbss..",
        "..sbuubbuububs..",
        "....bbbbbbbb....",
        "....ss....ss....",
        "....ss....ss....",
        "....xx....xx....",
    ];
    let male_img = ascii_to_image(male_ascii, &[
        ('h', h), ('l', h_l), ('s', s), ('d', s_d), ('w', w), ('x', x), ('b', b), ('u', d_b), ('.', _t)
    ]);

    // Female: 16x16 (Cavewoman - long wild hair, fur dress)
    let female_ascii = &[
        "................",
        "....h.hhhh.h....",
        "...hhhhhhhhhh...",
        "..hhhhhhhhhhhh..",
        "..hhhhsssshhhh..",
        "..hhhswsswshhh..",
        "..hhhhsxsxshhh..",
        "..hhhhhdssdhhh..",
        "..hhhhbbbbhhhh..",
        "..ssbbbbbbbbss..",
        "..ssbuubbuubss..",
        "..sbbbbbbbbbbs..",
        "..sbuubbuububs..",
        "....bbbbbbbb....",
        ".....ss..ss.....",
        ".....xx..xx.....",
    ];
    let female_img = ascii_to_image(female_ascii, &[
        ('h', h), ('l', h_l), ('s', s), ('w', w), ('x', x), ('d', s_d), ('b', b), ('u', d_b), ('.', _t)
    ]);

    // Plant Mature: 16x16 (Lush bush with fruit)
    let plant_ascii = &[
        "................",
        "......lll.......",
        "....lleeeel.....",
        "...leeeveeeel...",
        "..leeervreeeel..",
        ".leeeroreeveeeel",
        "lleeeeeeeeervrel",
        "leeevreeeeeeeeel",
        ".leeeeroreeevel.",
        "..lleeeeeeeell..",
        "...lleeeveell...",
        "....llleeell....",
        ".......d........",
        "......ddd.......",
        "......ddd.......",
        "......ddd.......",
    ];
    let plant_mature_img = ascii_to_image(plant_ascii, &[
        ('e', e), ('l', e_l), ('v', e_d), ('r', r), ('o', r_l), ('d', d_b), ('.', _t)
    ]);

    let plant_seed_ascii = &[
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        ".......dd.......",
        "......dddd......",
        "......dddd......",
        ".......dd.......",
    ];
    let plant_seed_img = ascii_to_image(plant_seed_ascii, &[('d', d_b), ('.', _t)]);

    let plant_seedling_ascii = &[
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        ".......ll.......",
        "......leel......",
        ".....leeeel.....",
        "....leeeeve.....",
        "......eeev......",
        ".......ev.......",
        ".......ev.......",
        ".......ev.......",
        ".......d........",
    ];
    let plant_seedling_img = ascii_to_image(plant_seedling_ascii, &[
        ('e', e), ('l', e_l), ('v', e_d), ('d', d_b), ('.', _t)
    ]);

    // Tree Seedling: 16x16
    let tree_seedling_ascii = &[
        "................",
        "................",
        "................",
        "................",
        "......eee.......",
        ".....eeeee......",
        "....eeeeeee.....",
        "....eeeeeee.....",
        ".....eeeee......",
        "......eee.......",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
    ];
    let tree_seedling_img = ascii_to_image(tree_seedling_ascii, &[
        ('e', e), ('d', d_b), ('.', _t)
    ]);

    // Tree Mature: 16x16
    let tree_mature_ascii = &[
        "......eee.......",
        "....eeeeeee.....",
        "...eeeeeeeee....",
        "..eeeeeeeeeee...",
        "..eeeeeeeeeee...",
        "..eeeeeeeeeee...",
        "...eeeeeeeee....",
        "....eeeeeee.....",
        ".....eeeee......",
        "......eee.......",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
        ".......d........",
    ];
    let tree_mature_img = ascii_to_image(tree_mature_ascii, &[
        ('e', e), ('d', d_b), ('.', _t)
    ]);

    // Camp: 16x16 (Simple hut)
    let camp_ascii = &[
        "................",
        ".......b........",
        "......bbb.......",
        ".....bbbbb......",
        "....bbbbbbb.....",
        "...bbbbbbbbb....",
        "..bbbbbbbbbbb...",
        ".bbbbbbbbbbbbb..",
        "bbbbbbbbbbbbbbb.",
        "bbbbbbb..bbbbbbb",
        "bbbbbb....bbbbbb",
        "bbbbb......bbbbb",
        "bbbb........bbbb",
        "bbb..........bbb",
        "bb............bb",
        "xx............xx",
    ];
    let camp_img = ascii_to_image(camp_ascii, &[
        ('b', b), ('x', x), ('.', _t)
    ]);

    // Bed: 16x10 — wooden frame (dark brown), mattress (tan), pillow (white)
    let p = PixelColor::new(220, 200, 170, 255); // Pillow/light tan
    let bed_ascii = &[
        "dddddddddddddddd",
        "duuuuuuuuuuuuuud",
        "duppppppuuuuuuud",
        "duppppppuuuuuuud",
        "duuuuuuuuuuuuuud",
        "duuuuuuuuuuuuuud",
        "duuuuuuuuuuuuuud",
        "duuuuuuuuuuuuuud",
        "duuuuuuuuuuuuuud",
        "dddddddddddddddd",
    ];
    let bed_img = ascii_to_image(bed_ascii, &[
        ('d', d_b), ('u', a), ('p', p), ('.', _t)
    ]);

    // Blueprint: 16x16 crossed wooden scaffold beams (under-construction marker)
    let sc  = PixelColor::new(210, 165, 80, 220);  // scaffold wood (golden tan)
    let sc2 = PixelColor::new(140, 100, 40, 220);  // darker beam shadow
    let blueprint_ascii = &[
        "sc............cs",
        "csc..........csc",
        ".csc........csc.",
        "..csc......csc..",
        "...csc....csc...",
        "....csc..csc....",
        ".....csccsc.....",
        "......scsc......",
        "......scsc......",
        ".....csc.csc....",
        "....csc...csc...",
        "...csc.....csc..",
        "..csc.......csc.",
        ".csc.........csc",
        "csc...........sc",
        "sc............cs",
    ];
    let blueprint_img = ascii_to_image(blueprint_ascii, &[
        ('s', sc), ('c', sc2), ('.', _t)
    ]);

    // Wall: 16x16 (Solid stone block with some texture)
    let wall_ascii = &[
        "dddddddddddddddd",
        "dggggggggggggggd",
        "dglllllgllllllgd",
        "dglllllgllllllgd",
        "dglllllgllllllgd",
        "dggggggggggggggd",
        "dgglllllllgllllg",
        "dgglllllllgllllg",
        "dgglllllllgllllg",
        "dggggggggggggggd",
        "dglllllgllllllgd",
        "dglllllgllllllgd",
        "dglllllgllllllgd",
        "dggggggggggggggd",
        "dggggggggggggggd",
        "xxxxxxxxxxxxxxxx",
    ];
    let wall_img = ascii_to_image(wall_ascii, &[
        ('g', g), ('d', d), ('l', l_g), ('x', x), ('.', _t)
    ]);

    commands.insert_resource(EntityTextures {
        wolf: images.add(wolf_img),
        deer: images.add(deer_img),
        person_male: images.add(male_img),
        person_female: images.add(female_img),
        plant_seed: images.add(plant_seed_img),
        plant_seedling: images.add(plant_seedling_img),
        plant_mature: images.add(plant_mature_img),
        tree_seedling: images.add(tree_seedling_img),
        tree_mature: images.add(tree_mature_img),
        camp: images.add(camp_img),
        bed: images.add(bed_img),
        blueprint: images.add(blueprint_img),
        wall: images.add(wall_img),
    });
}
