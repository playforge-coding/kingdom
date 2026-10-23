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

    /// Mean color of a static sprite's opaque pixels, in linear RGBA (the space
    /// the shader blends in, since the atlas is sRGB). The zoomed-out map paints
    /// flat tiles in these so it reads like the textures it stands in for.
    pub fn average_color(&self, name: &str) -> [f32; 4] {
        let r = self.uv(name);
        let (x0, y0) = (
            (r[0] * self.width as f32).round() as u32,
            (r[1] * self.height as f32).round() as u32,
        );
        let (x1, y1) = (
            (r[2] * self.width as f32).round() as u32,
            (r[3] * self.height as f32).round() as u32,
        );
        let to_linear = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let mut sum = [0.0f32; 3];
        let mut n = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * self.width + x) * 4) as usize;
                if self.rgba[i + 3] < 128 {
                    continue;
                }
                for (s, &c) in sum.iter_mut().zip(&self.rgba[i..i + 3]) {
                    *s += to_linear(c);
                }
                n += 1;
            }
        }
        let n = n.max(1) as f32;
        [sum[0] / n, sum[1] / n, sum[2] / n, 1.0]
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

/// Flat colors (linear RGBA) for drawing the map without textures: the
/// zoomed-out view and the minimap.
#[derive(Clone, Copy)]
pub struct MapColors {
    pub grass: [f32; 4],
    pub water: [f32; 4],
    pub tree: [f32; 4],
    pub rock: [f32; 4],
    /// Chunks that haven't been generated yet.
    pub unexplored: [f32; 4],
    pub player: [f32; 4],
    pub enemy: [f32; 4],
    pub ally: [f32; 4],
}

impl MapColors {
    pub fn from_atlas(atlas: &Atlas) -> Self {
        let grass = atlas.average_color("grass");
        // A tree or rock only covers part of its tile, so blend it with the
        // grass showing around it.
        let over_grass = |c: [f32; 4]| {
            let mut out = [1.0; 4];
            for i in 0..3 {
                out[i] = grass[i] + (c[i] - grass[i]) * 0.6;
            }
            out
        };
        MapColors {
            grass,
            water: atlas.average_color("water"),
            tree: over_grass(atlas.average_color("tree")),
            rock: over_grass(atlas.average_color("rock")),
            unexplored: [0.012, 0.014, 0.02, 1.0],
            // The same hues as the HP bars.
            player: [0.3, 0.55, 1.0, 1.0],
            enemy: [0.9, 0.25, 0.2, 1.0],
            ally: [0.25, 0.85, 0.3, 1.0],
        }
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
    let ally_houses = load(include_bytes!("../assets/textures/tiles/ally_houses.png"));
    let bridge = load(include_bytes!("../assets/textures/tiles/bridge.png"));
    let wall = load(include_bytes!("../assets/textures/tiles/wall.png"));
    let tree = load(include_bytes!("../assets/textures/tiles/tree.png"));
    let rock = load(include_bytes!("../assets/textures/tiles/rock.png"));
    let rally = load(include_bytes!("../assets/textures/tiles/rally.png"));
    let cave = load(include_bytes!("../assets/textures/tiles/cave.png"));
    let hut = load(include_bytes!("../assets/textures/tiles/hut.png"));
    let enemy_hut = load(include_bytes!("../assets/textures/tiles/enemy_hut.png"));
    let ally_hut = load(include_bytes!("../assets/textures/tiles/ally_hut.png"));

    let farmer = load(include_bytes!("../assets/textures/entities/farmer.png"));
    let knight = load(include_bytes!("../assets/textures/entities/swordsman.png"));
    let enemy_farmer = load(include_bytes!(
        "../assets/textures/entities/enemy_farmer.png"
    ));
    let enemy_knight = load(include_bytes!(
        "../assets/textures/entities/enemy_swordsman.png"
    ));
    let ally_farmer = load(include_bytes!(
        "../assets/textures/entities/ally_farmer.png"
    ));
    let ally_knight = load(include_bytes!(
        "../assets/textures/entities/ally_swordsman.png"
    ));
    let horseman = load(include_bytes!("../assets/textures/entities/horseman.png"));
    let enemy_horseman = load(include_bytes!(
        "../assets/textures/entities/enemy_horseman.png"
    ));
    let ally_horseman = load(include_bytes!(
        "../assets/textures/entities/ally_horseman.png"
    ));
    let bowman = load(include_bytes!("../assets/textures/entities/bowman.png"));
    let enemy_bowman = load(include_bytes!(
        "../assets/textures/entities/enemy_bowman.png"
    ));
    let ally_bowman = load(include_bytes!(
        "../assets/textures/entities/ally_bowman.png"
    ));
    let arrow = load(include_bytes!("../assets/textures/entities/arrow.png"));
    let cargo_ship = load(include_bytes!("../assets/textures/entities/cargo_ship.png"));
    let warship = load(include_bytes!("../assets/textures/entities/warship.png"));
    let enemy_warship = load(include_bytes!(
        "../assets/textures/entities/enemy_warship.png"
    ));
    let ally_warship = load(include_bytes!(
        "../assets/textures/entities/ally_warship.png"
    ));
    let pirate_ship = load(include_bytes!(
        "../assets/textures/entities/pirate_ship.png"
    ));
    let cannonball = load(include_bytes!("../assets/textures/entities/cannonball.png"));

    let sources = vec![
        Source::sprite("white", white_pixel()),
        Source::sprite("grass", grass),
        Source::sprite("water", water),
        // 3x3 grids of 16x16 buildings; use the top-left one.
        Source::sprite("house", crop(&houses, 0, 0, 16, 16)),
        Source::sprite("enemy_house", crop(&enemy_houses, 0, 0, 16, 16)),
        Source::sprite("ally_house", crop(&ally_houses, 0, 0, 16, 16)),
        // The bridge sprite spans north to south; a quarter turn gives the
        // one laid across an east-west crossing.
        Source::sprite("bridge_ew", image::imageops::rotate90(&bridge)),
        Source::sprite("bridge", bridge),
        Source::sprite("wall", wall),
        Source::sprite("tree", pad_to_tile(&tree)),
        Source::sprite("rock", pad_to_tile(&rock)),
        Source::sprite("rally", rally),
        Source::sprite("cave", cave),
        Source::sprite("hut", hut),
        Source::sprite("enemy_hut", enemy_hut),
        Source::sprite("ally_hut", ally_hut),
        // Character sheets: 5 columns x 12 rows of 16x16 frames.
        Source::sheet("farmer", farmer, 5, 12),
        Source::sheet("knight", knight, 5, 12),
        Source::sheet("enemy_farmer", enemy_farmer, 5, 12),
        Source::sheet("enemy_knight", enemy_knight, 5, 12),
        Source::sheet("ally_farmer", ally_farmer, 5, 12),
        Source::sheet("ally_knight", ally_knight, 5, 12),
        // Horseman sheets: 4 columns x 12 rows of 32x32 frames, three rows
        // per facing (down, right, left, up): two of riding, one of the lance.
        Source::sheet("horseman", horseman, 4, 12),
        Source::sheet("enemy_horseman", enemy_horseman, 4, 12),
        Source::sheet("ally_horseman", ally_horseman, 4, 12),
        // Bowman sheets: 5 columns x 8 rows of 16x16 frames, four rows of
        // walking then four of drawing the bow (down, up, left, right).
        Source::sheet("bowman", bowman, 5, 8),
        Source::sheet("enemy_bowman", enemy_bowman, 5, 8),
        Source::sheet("ally_bowman", ally_bowman, 5, 8),
        // Arrow: 1 column x 2 rows of 16x16 frames, pointing up then right.
        Source::sheet("arrow", arrow, 1, 2),
        // Cargo ship: 4 columns x 1 row of 32x32 frames, facing left, up, down,
        // right (in that column order).
        Source::sheet("cargo_ship", cargo_ship, 4, 1),
        // Warship: 4 columns x 1 row of 32x32 frames, facing left, up, down,
        // right (same column order as the other ships).
        Source::sheet("warship", warship, 4, 1),
        // The enemy's and the allies' hulls, same layout: enemy warships and
        // raid ships wear the red one, allied escorts and the mercenaries'
        // ships the green one.
        Source::sheet("enemy_warship", enemy_warship, 4, 1),
        Source::sheet("ally_warship", ally_warship, 4, 1),
        // Pirate ship: 4 columns x 1 row of 32x32 frames, facing left, up,
        // down, right (in that column order).
        Source::sheet("pirate_ship", pirate_ship, 4, 1),
        // Cannonball: a single small sprite.
        Source::sprite("cannonball", cannonball),
    ];

    pack(sources)
}

/// Simple shelf packer with 1px padding to avoid bleeding between sprites.
fn pack(mut sources: Vec<Source>) -> Atlas {
    const PAD: u32 = 1;
    const ATLAS_W: u32 = 512;

    sources.sort_by_key(|s| std::cmp::Reverse(s.img.height()));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hull_the_renderer_draws_is_packed() {
        // The renderer picks a ship's sheet by its owner, so each faction's
        // hull must be in the atlas or the first enemy raid would panic
        // mid-frame rather than here.
        let atlas = build();
        for sheet in [
            "cargo_ship",
            "warship",
            "enemy_warship",
            "ally_warship",
            "pirate_ship",
        ] {
            for col in 0..4 {
                let uv = atlas.frame_uv(sheet, col, 0);
                assert!(
                    uv[2] > uv[0] && uv[3] > uv[1],
                    "{sheet} frame {col} is empty"
                );
            }
        }
    }

    #[test]
    fn every_rider_sheet_is_packed_with_all_twelve_rows() {
        // The renderer reads a horseman's frame by facing block and row, so
        // each faction's sheet must be packed with its full 4x12 grid.
        let atlas = build();
        for sheet in ["horseman", "enemy_horseman", "ally_horseman"] {
            for row in 0..12 {
                for col in 0..4 {
                    let uv = atlas.frame_uv(sheet, col, row);
                    assert!(
                        uv[2] > uv[0] && uv[3] > uv[1],
                        "{sheet} frame ({col}, {row}) is empty"
                    );
                }
            }
        }
    }

    #[test]
    fn every_bowman_sheet_is_packed_with_all_eight_rows() {
        // The renderer reads a bowman's frame by state block and facing, so
        // each faction's sheet must be packed with its full 5x8 grid, and the
        // arrow with both of its headings.
        let atlas = build();
        for sheet in ["bowman", "enemy_bowman", "ally_bowman"] {
            for row in 0..8 {
                for col in 0..5 {
                    let uv = atlas.frame_uv(sheet, col, row);
                    assert!(
                        uv[2] > uv[0] && uv[3] > uv[1],
                        "{sheet} frame ({col}, {row}) is empty"
                    );
                }
            }
        }
        for row in 0..2 {
            let uv = atlas.frame_uv("arrow", 0, row);
            assert!(uv[2] > uv[0] && uv[3] > uv[1], "arrow frame {row} is empty");
        }
    }
}
