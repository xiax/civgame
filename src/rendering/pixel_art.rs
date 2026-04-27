use bevy::prelude::*;
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

#[derive(Resource, PartialEq, Eq, Clone, Copy, Debug)]
pub enum ArtMode {
    Ascii,
    Pixel,
}

impl Default for ArtMode {
    fn default() -> Self {
        ArtMode::Pixel
    }
}

#[derive(Resource)]
pub struct EntityTextures {
    // ASCII Handles
    pub wolf_ascii: Handle<Image>,
    pub deer_ascii: Handle<Image>,
    pub person_male_ascii: Handle<Image>,
    pub person_female_ascii: Handle<Image>,
    pub plant_seed_ascii: Handle<Image>,
    pub plant_seedling_ascii: Handle<Image>,
    pub plant_mature_ascii: Handle<Image>,
    pub tree_seedling_ascii: Handle<Image>,
    pub tree_mature_ascii: Handle<Image>,
    pub camp_ascii: Handle<Image>,
    pub bed_ascii: Handle<Image>,
    pub blueprint_ascii: Handle<Image>,
    pub wall_ascii: Handle<Image>,
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
    let d = PixelColor::new(80, 80, 85, 255); // Dark Gray
    let l_g = PixelColor::new(190, 190, 195, 255); // Light Gray

    // Browns (Deer/Wood/Dirt)
    let b = PixelColor::new(120, 80, 40, 255); // Brown
    let d_b = PixelColor::new(70, 45, 20, 255); // Dark Brown
    let l_b = PixelColor::new(160, 110, 60, 255); // Light Brown
    let a = PixelColor::new(220, 200, 170, 255); // Tan/Antler

    // Skin/Hair
    let s = PixelColor::new(255, 215, 190, 255); // Skin
    let s_d = PixelColor::new(230, 170, 140, 255); // Skin Shadow
    let h = PixelColor::new(85, 60, 40, 255); // Hair
    let h_l = PixelColor::new(120, 95, 60, 255); // Hair Highlight

    // Nature
    let e = PixelColor::new(50, 160, 50, 255); // Green (Plant)
    let e_d = PixelColor::new(30, 100, 30, 255); // Dark Green
    let e_l = PixelColor::new(120, 220, 80, 255); // Light Green
    let r = PixelColor::new(230, 30, 30, 255); // Red (Fruit)
    let r_l = PixelColor::new(255, 90, 90, 255); // Light Red

    // Basics
    let w = PixelColor::new(255, 255, 255, 255); // White
    let y = PixelColor::new(255, 240, 40, 255); // Yellow (Eyes/Gold)
    let x = PixelColor::new(20, 20, 20, 255); // Black/Darkest

    // Wolf: 16x16
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
    let wolf_img = ascii_to_image(
        wolf_ascii,
        &[
            ('g', g),
            ('d', d),
            ('l', l_g),
            ('y', y),
            ('w', w),
            ('x', x),
            ('.', _t),
        ],
    );

    // Deer: 16x16
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
    let deer_img = ascii_to_image(
        deer_ascii,
        &[
            ('l', l_b),
            ('b', b),
            ('d', d_b),
            ('a', a),
            ('x', x),
            ('w', w),
            ('h', l_b),
            ('.', _t),
        ],
    );

    // Male: 16x16
    let male_ascii = &[
        "................",
        "......xxxx......",
        ".....xhhhhx.....",
        "....xhhhhhhx....",
        "....xhwxswxx....",
        "....xhsssssxx...",
        "....xhsssssxx...",
        ".....xxsssxxx...",
        ".....xbbbbbxx...",
        "....xbbbbbbbxx..",
        "....xsssssssx...",
        "....xsssssssx...",
        "....xxsxssxx....",
        ".....xx.xx......",
        ".....xx.xx......",
        "................",
    ];
    let male_img = ascii_to_image(
        male_ascii,
        &[
            ('h', h),
            ('l', h_l),
            ('s', s),
            ('d', s_d),
            ('w', w),
            ('x', x),
            ('b', b),
            ('u', d_b),
            ('.', _t),
        ],
    );

    // Female: 16x16
    let female_ascii = &[
        "................",
        "......xxxx......",
        ".....xhhhhx.....",
        "....xhhhhhhx....",
        "....xhwxswxx....",
        "....xhsssssxx...",
        "...xxhsssssxx...",
        "...xhhsssxxx....",
        "...xhbbbbbxx....",
        "...xhbbbbbbbxx..",
        "....xsssssssx...",
        "....xsssssssx...",
        "....xxsxssxx....",
        ".....xx.xx......",
        ".....xx.xx......",
        "................",
    ];
    let female_img = ascii_to_image(
        female_ascii,
        &[
            ('h', h),
            ('l', h_l),
            ('s', s),
            ('w', w),
            ('x', x),
            ('d', s_d),
            ('b', b),
            ('u', d_b),
            ('.', _t),
        ],
    );

    // Plant Mature: 16x16
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
    let plant_mature_img = ascii_to_image(
        plant_ascii,
        &[
            ('e', e),
            ('l', e_l),
            ('v', e_d),
            ('r', r),
            ('o', r_l),
            ('d', d_b),
            ('.', _t),
        ],
    );

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
    let plant_seedling_img = ascii_to_image(
        plant_seedling_ascii,
        &[('e', e), ('l', e_l), ('v', e_d), ('d', d_b), ('.', _t)],
    );

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
    let tree_seedling_img = ascii_to_image(tree_seedling_ascii, &[('e', e), ('d', d_b), ('.', _t)]);

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
    let tree_mature_img = ascii_to_image(tree_mature_ascii, &[('e', e), ('d', d_b), ('.', _t)]);

    // Camp: 16x16
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
    let camp_img = ascii_to_image(camp_ascii, &[('b', b), ('x', x), ('.', _t)]);

    // Bed: 16x10
    let p = PixelColor::new(220, 200, 170, 255);
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
    let bed_img = ascii_to_image(bed_ascii, &[('d', d_b), ('u', a), ('p', p), ('.', _t)]);

    // Blueprint: 16x16
    let sc = PixelColor::new(210, 165, 80, 220);
    let sc2 = PixelColor::new(140, 100, 40, 220);
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
    let blueprint_img = ascii_to_image(blueprint_ascii, &[('s', sc), ('c', sc2), ('.', _t)]);

    // Wall: 16x16
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
    let wall_img = ascii_to_image(
        wall_ascii,
        &[('g', g), ('d', d), ('l', l_g), ('x', x), ('.', _t)],
    );

    commands.insert_resource(ArtMode::default());

    commands.insert_resource(EntityTextures {
        wolf_ascii: images.add(wolf_img),
        deer_ascii: images.add(deer_img),
        person_male_ascii: images.add(male_img),
        person_female_ascii: images.add(female_img),
        plant_seed_ascii: images.add(plant_seed_img),
        plant_seedling_ascii: images.add(plant_seedling_img),
        plant_mature_ascii: images.add(plant_mature_img),
        tree_seedling_ascii: images.add(tree_seedling_img),
        tree_mature_ascii: images.add(tree_mature_img),
        camp_ascii: images.add(camp_img),
        bed_ascii: images.add(bed_img),
        blueprint_ascii: images.add(blueprint_img),
        wall_ascii: images.add(wall_img),
    });
}
