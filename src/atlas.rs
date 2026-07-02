//! Builds a single texture atlas at startup by packing all sprites (loaded from
//! embedded PNGs) into one RGBA8 image. Everything is embedded with
//! `include_bytes!` so the exact same code path works on native and web.
//!
//! Static sprites (tiles, buildings) are stored as single UV rects. Character
//! sprite sheets are stored whole, with their grid dimensions, so we can slice
//! out individual animation frames at render time.

use std::collections::HashMap;

use image::{GenericImage, GenericImageView, RgbaImage};

/// UV rectangle into the atlas: [u_min, v_min, u_max, v_max].
pub type UvRect = [f32; 4];

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    rects: HashMap<&'static str, UvRect>,
    /// name -> (whole-sheet uv rect, cols, rows)
    sheets: HashMap<&'static str, (UvRect, u32, u32)>,
}

impl Atlas {
    pub fn uv(&self, name: &str) -> UvRect {
        *self
            .rects
            .get(name)
            .unwrap_or_else(|| panic!("sprite '{name}' not in atlas"))
    }

    /// UV rect for a single frame (col, row) of a packed sprite sheet.
    pub fn frame_uv(&self, sheet: &str, col: u32, row: u32) -> UvRect {
        let (r, cols, rows) = self
            .sheets
            .get(sheet)
            .unwrap_or_else(|| panic!("sheet '{sheet}' not in atlas"));
        let fw = (r[2] - r[0]) / *cols as f32;
        let fh = (r[3] - r[1]) / *rows as f32;
        let x = r[0] + col as f32 * fw;
        let y = r[1] + row as f32 * fh;
        [x, y, x + fw, y + fh]
    }
}

struct Source {
    name: &'static str,
    img: RgbaImage,
    /// `Some((cols, rows))` marks this as an animation sheet.
    grid: Option<(u32, u32)>,
}

impl Source {
    fn sprite(name: &'static str, img: RgbaImage) -> Self {
        Source {
            name,
            img,
            grid: None,
        }
    }
    fn sheet(name: &'static str, img: RgbaImage, cols: u32, rows: u32) -> Self {
        Source {
            name,
            img,
            grid: Some((cols, rows)),
        }
    }
}

fn load(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("embedded PNG failed to decode")
        .to_rgba8()
}

fn crop(img: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> RgbaImage {
    img.view(x, y, w, h).to_image()
}

fn white_pixel() -> RgbaImage {
    RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]))
}

/// Center a smaller sprite (e.g. the 14x14 tree) onto a transparent 16x16 tile
/// footprint, bottom-aligned so objects "sit" on their tile.
fn pad_to_tile(src: &RgbaImage) -> RgbaImage {
    const TILE: u32 = 16;
    let mut out = RgbaImage::from_pixel(TILE, TILE, image::Rgba([0, 0, 0, 0]));
    let ox = (TILE.saturating_sub(src.width())) / 2;
    let oy = TILE.saturating_sub(src.height()); // bottom-aligned
    out.copy_from(src, ox, oy).ok();
    out
}

pub fn build() -> Atlas {
    let grass = load(include_bytes!("../assets/textures/tiles/grass.png"));
    let water = load(include_bytes!("../assets/textures/tiles/water.png"));
    let houses = load(include_bytes!("../assets/textures/tiles/houses.png"));
    let enemy_houses = load(include_bytes!("../assets/textures/tiles/enemy_houses.png"));
    let bridge = load(include_bytes!("../assets/textures/tiles/bridge.png"));
    let wall = load(include_bytes!("../assets/textures/tiles/wall.png"));
    let tree = load(include_bytes!("../assets/textures/tiles/tree.png"));
    let rock = load(include_bytes!("../assets/textures/tiles/rock.png"));
    let rally = load(include_bytes!("../assets/textures/tiles/rally.png"));

    let farmer = load(include_bytes!("../assets/textures/entities/farmer.png"));
    let knight = load(include_bytes!("../assets/textures/entities/swordsman.png"));
    let enemy_farmer = load(include_bytes!(
        "../assets/textures/entities/enemy_farmer.png"
    ));
    let enemy_knight = load(include_bytes!(
        "../assets/textures/entities/enemy_swordsman.png"
    ));

    let sources = vec![
        Source::sprite("white", white_pixel()),
        Source::sprite("grass", grass),
        Source::sprite("water", water),
        // 3x3 grids of 16x16 buildings; use the top-left one.
        Source::sprite("house", crop(&houses, 0, 0, 16, 16)),
        Source::sprite("enemy_house", crop(&enemy_houses, 0, 0, 16, 16)),
        Source::sprite("bridge", bridge),
        Source::sprite("wall", wall),
        Source::sprite("tree", pad_to_tile(&tree)),
        Source::sprite("rock", pad_to_tile(&rock)),
        Source::sprite("rally", rally),
        // Character sheets: 5 columns x 12 rows of 16x16 frames.
        Source::sheet("farmer", farmer, 5, 12),
        Source::sheet("knight", knight, 5, 12),
        Source::sheet("enemy_farmer", enemy_farmer, 5, 12),
        Source::sheet("enemy_knight", enemy_knight, 5, 12),
    ];

    pack(sources)
}

/// Simple shelf packer with 1px padding to avoid bleeding between sprites.
fn pack(mut sources: Vec<Source>) -> Atlas {
    const PAD: u32 = 1;
    const ATLAS_W: u32 = 512;

    sources.sort_by(|a, b| b.img.height().cmp(&a.img.height()));

    let mut cursor_x = PAD;
    let mut cursor_y = PAD;
    let mut shelf_h = 0u32;
    let mut placements: Vec<(usize, u32, u32)> = Vec::new();

    for (i, s) in sources.iter().enumerate() {
        let (w, h) = (s.img.width(), s.img.height());
        if cursor_x + w + PAD > ATLAS_W {
            cursor_x = PAD;
            cursor_y += shelf_h + PAD;
            shelf_h = 0;
        }
        placements.push((i, cursor_x, cursor_y));
        cursor_x += w + PAD;
        shelf_h = shelf_h.max(h);
    }

    let atlas_h = (cursor_y + shelf_h + PAD).next_power_of_two();
    let mut atlas = RgbaImage::from_pixel(ATLAS_W, atlas_h, image::Rgba([0, 0, 0, 0]));
    let mut rects = HashMap::new();
    let mut sheets = HashMap::new();

    for (i, x, y) in placements {
        let src = &sources[i];
        atlas.copy_from(&src.img, x, y).unwrap();
        let (w, h) = (src.img.width(), src.img.height());
        let rect = [
            x as f32 / ATLAS_W as f32,
            y as f32 / atlas_h as f32,
            (x + w) as f32 / ATLAS_W as f32,
            (y + h) as f32 / atlas_h as f32,
        ];
        match src.grid {
            Some((cols, rows)) => {
                sheets.insert(src.name, (rect, cols, rows));
            }
            None => {
                rects.insert(src.name, rect);
            }
        }
    }

    Atlas {
        width: ATLAS_W,
        height: atlas_h,
        rgba: atlas.into_raw(),
        rects,
        sheets,
    }
}
