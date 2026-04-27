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
    pub plant_grain_mature_ascii: Handle<Image>,
    pub plant_bush_mature_ascii: Handle<Image>,
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

pub const WARM_PALETTE: &[(char, PixelColor)] = &[
    ('.', PixelColor::new(0,   0,   0,   0  )), // transparent
    ('X', PixelColor::new(26,  20,  16,  255)), // near-black outline
    ('d', PixelColor::new(42,  31,  23,  255)), // very dark brown
    ('D', PixelColor::new(74,  52,  34,  255)), // dark brown
    ('b', PixelColor::new(122, 84,  54,  255)), // mid brown
    ('B', PixelColor::new(168, 114, 70,  255)), // light brown / tan skin
    ('t', PixelColor::new(212, 165, 116, 255)), // tan / wood highlight
    ('T', PixelColor::new(240, 213, 168, 255)), // pale tan / parchment
    ('W', PixelColor::new(255, 243, 214, 255)), // warm cream
    ('s', PixelColor::new(58,  42,  26,  255)), // dark soil
    ('S', PixelColor::new(94,  63,  36,  255)), // soil / earth
    ('e', PixelColor::new(139, 90,  43,  255)), // earth / dirt path
    ('E', PixelColor::new(192, 136, 85,  255)), // sand / clay
    ('N', PixelColor::new(232, 193, 137, 255)), // pale sand
    ('g', PixelColor::new(31,  58,  28,  255)), // deep forest green
    ('G', PixelColor::new(54,  94,  42,  255)), // mossy green
    ('m', PixelColor::new(90,  138, 58,  255)), // grass green
    ('M', PixelColor::new(140, 186, 79,  255)), // bright grass
    ('L', PixelColor::new(184, 217, 106, 255)), // grass highlight
    ('n', PixelColor::new(26,  40,  64,  255)), // deep water
    ('i', PixelColor::new(45,  79,  122, 255)), // water mid
    ('I', PixelColor::new(74,  130, 184, 255)), // water highlight
    ('H', PixelColor::new(140, 192, 224, 255)), // water foam / sky
    ('k', PixelColor::new(68,  74,  82,  255)), // slate dark
    ('K', PixelColor::new(107, 114, 124, 255)), // slate mid
    ('l', PixelColor::new(154, 160, 168, 255)), // slate light
    ('P', PixelColor::new(212, 214, 216, 255)), // stone highlight / snow
    ('r', PixelColor::new(122, 31,  31,  255)), // blood red / banner
    ('R', PixelColor::new(200, 74,  42,  255)), // fire / terracotta
    ('o', PixelColor::new(245, 168, 60,  255)), // flame / gold
    ('y', PixelColor::new(252, 230, 112, 255)), // bright gold / spark
    ('p', PixelColor::new(90,  42,  110, 255)), // royal purple / magic
];

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

    // Plant Mature — Grain (wheat field): 16x16
    let plant_grain_mature_img = ascii_to_image(&[
        "...y...y...y....",
        "..yoy.yoy.yoy...",
        ".yoyoyoyoyoyoy..",
        "..yXy.yXy.yXy...",
        "...X...X...X....",
        "...X...X...X....",
        "...X...X...X....",
        "...y...y...y....",
        "..yoy.yoy.yoy...",
        ".yoyoyoyoyoyoy..",
        "..yXy.yXy.yXy...",
        "...X...X...X....",
        "...X...X...X....",
        "...X...X...X....",
        "..mLmmMmmLmmMmm.",
        "..MmmLmmMmmLmMm.",
    ], WARM_PALETTE);

    // Plant Mature — BerryBush (berry bush): 16x16
    let plant_bush_mature_img = ascii_to_image(&[
        "................",
        "................",
        "................",
        "................",
        "......GgmG......",
        ".....GmrRmG.....",
        "....GgmGgmGm....",
        "...GmGrRmGmGm...",
        "..GgrRmGgmGgmG..",
        "..GmGmrRrGmGmG..",
        "..GgmGmGmGgrRm..",
        "...GmGgrRrGmGm..",
        "....GmGgmGmGm...",
        ".....GmGmGmG....",
        "......GmGmG.....",
        "......XdSdX.....",
    ], WARM_PALETTE);

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

    // Plant Seedling (sapling): 16x16
    let plant_seedling_img = ascii_to_image(&[
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "......XGmX......",
        "....XGmGmG......",
        "....XmGgmG......",
        "......XGX.......",
        "......XGX.......",
        "......XGX.......",
        "......XGX.......",
        "......XdX.......",
        ".....XdSdX......",
    ], WARM_PALETTE);

    // Tree Seedling (sapling): 16x16
    let tree_seedling_img = ascii_to_image(&[
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "......XGmX......",
        "....XGmGmG......",
        "....XmGgmG......",
        "......XGX.......",
        "......XGX.......",
        "......XGX.......",
        "......XGX.......",
        "......XdX.......",
        ".....XdSdX......",
    ], WARM_PALETTE);

    // Tree Mature (oak): 16x32 tall sprite
    let tree_mature_img = ascii_to_image(&[
        "................",
        "......GgmG......",
        "....GgmGgmGGm...",
        "...GmGgmGmGmGg..",
        "..GmGgmGmGmGgmG.",
        ".GgmGgmGmGmGgmGg",
        ".GmGgmGmGmGgmGmG",
        "GgmGmGmGmGmGmGgm",
        "GmGgmGmGmGmGgmGm",
        "GgmGmGmGgmGmGmGg",
        ".GmGgmGmGmGgmGmG",
        ".GgmGmGmGmGgmGg.",
        "..GmGgmGmGgmGmG.",
        "...GgmGmGgmGmG..",
        "....GmGgmGgmG...",
        "......GmGmG.....",
        ".......XbX......",
        ".......XbX......",
        "......XbBX......",
        "......XbtbX.....",
        "......XbtbX.....",
        "......XdSdX.....",
        ".....XdSSdX.....",
        "....XdSSSSdX....",
        "...XSSSSSSSX....",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
        "................",
    ], WARM_PALETTE);

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
        plant_grain_mature_ascii: images.add(plant_grain_mature_img),
        plant_bush_mature_ascii: images.add(plant_bush_mature_img),
        tree_seedling_ascii: images.add(tree_seedling_img),
        tree_mature_ascii: images.add(tree_mature_img),
        camp_ascii: images.add(camp_img),
        bed_ascii: images.add(bed_img),
        blueprint_ascii: images.add(blueprint_img),
        wall_ascii: images.add(wall_img),
    });
}
