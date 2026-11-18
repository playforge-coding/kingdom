//! Builds a single texture atlas at startup by packing all sprites (loaded from
//! embedded PNGs) into one RGBA8 image. Everything is embedded with
//! `include_bytes!` so the exact same code path works on native and web.
//!
//! Static sprites (tiles, buildings) are stored as single UV rects. Character
//! sprite sheets are stored whole, with their grid dimensions, so we can slice
//! out individual animation frames at render time.

use std::borrow::Cow;
use std::collections::HashMap;

use image::{GenericImage, GenericImageView, RgbaImage};

/// UV rectangle into the atlas: [u_min, v_min, u_max, v_max].
pub type UvRect = [f32; 4];

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// How many realms this atlas holds sprites for (see `realm_sheet`).
    pub realms: u8,
    rects: HashMap<String, UvRect>,
    /// name -> (whole-sheet uv rect, cols, rows)
    sheets: HashMap<String, (UvRect, u32, u32)>,
}

/// The sprites that come in a per-realm color: the base name of each unit
/// sheet, hull and building the atlas recolors for realms past the first.
const REALM_SPRITES: [&str; 8] = [
    "farmer", "knight", "horseman", "bowman", "axeman", "warship", "house", "hut",
];

/// The name of `base`'s sprite in `realm`'s color: the plain name for the
/// first realm, whose art is the original blue, and a recolored copy for
/// every realm after it.
pub fn realm_sheet(realm: u8, base: &'static str) -> Cow<'static, str> {
    if realm == 0 {
        Cow::Borrowed(base)
    } else {
        Cow::Owned(format!("realm{realm}_{base}"))
    }
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
    /// The color a realm's units, houses and flags are drawn in on the map
    /// and the minimap: the classic blue for the first realm, and for every
    /// further one a hue stepped around the wheel, skipping the enemy's red
    /// and the allies' green so no realm is mistaken for either. Any number
    /// of realms get a color; neighbors on the wheel stay far apart.
    pub fn realm(&self, realm: u8) -> [f32; 4] {
        if realm == 0 {
            return self.player;
        }
        realm_hue_rgb(realm_hue(realm))
    }
}

/// The hue, in degrees, a realm past the first is drawn in: a golden-angle
/// walk from orange, re-stepped past any hue within reach of the enemy's red
/// (0) or the allies' green (120), so successive realms land far apart and
/// never on a faction color.
fn realm_hue(realm: u8) -> f32 {
    const GOLDEN: f32 = 137.508;
    let near = |h: f32, c: f32| {
        let d = (h - c).rem_euclid(360.0);
        d.min(360.0 - d) < 28.0
    };
    let mut h = 35.0;
    let mut picked = 0;
    loop {
        if !near(h, 0.0) && !near(h, 120.0) {
            picked += 1;
            if picked == realm {
                return h;
            }
        }
        h = (h + GOLDEN).rem_euclid(360.0);
    }
}

/// A bright, fully saturated color of the given hue, in the linear RGBA the
/// map's flat colors use (matching the player's blue in lightness).
fn realm_hue_rgb(hue: f32) -> [f32; 4] {
    let h = hue / 60.0;
    let x = 1.0 - (h.rem_euclid(2.0) - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    // Lift toward white a little so dark hues (blues, purples) read on the
    // map the way the player's own blue does.
    let lift = |c: f32| 0.25 + 0.75 * c;
    [lift(r), lift(g), lift(b), 1.0]
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
    name: String,
    img: RgbaImage,
    /// `Some((cols, rows))` marks this as an animation sheet.
    grid: Option<(u32, u32)>,
}

impl Source {
    fn sprite(name: impl Into<String>, img: RgbaImage) -> Self {
        Source {
            name: name.into(),
            img,
            grid: None,
        }
    }
    fn sheet(name: impl Into<String>, img: RgbaImage, cols: u32, rows: u32) -> Self {
        Source {
            name: name.into(),
            img,
            grid: Some((cols, rows)),
        }
    }

    /// This source's art in `realm`'s color (see `recolor`), named for it.
    fn for_realm(&self, enemy: &RgbaImage, realm: u8) -> Self {
        Source {
            name: realm_sheet(realm, "").into_owned() + &self.name,
            img: recolor(&self.img, enemy, realm_hue(realm)),
            grid: self.grid,
        }
    }
}

/// `base`'s art with its team color swapped for `hue`. The team-colored
/// pixels are found by comparing the sprite with the enemy's copy of it,
/// which differs exactly where the clothing, banners and paintwork are;
/// each such pixel keeps its own shade and only changes hue, so the shading
/// of the original art carries over to every realm's color.
fn recolor(base: &RgbaImage, enemy: &RgbaImage, hue: f32) -> RgbaImage {
    let mut out = base.clone();
    for (x, y, px) in out.enumerate_pixels_mut() {
        if px[3] == 0 {
            continue;
        }
        let other = enemy.get_pixel(x, y);
        if px[0] == other[0] && px[1] == other[1] && px[2] == other[2] {
            continue;
        }
        let (_, s, l) = rgb_to_hsl(px[0], px[1], px[2]);
        let (r, g, b) = hsl_to_rgb(hue, s, l);
        *px = image::Rgba([r, g, b, px[3]]);
    }
    out
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to8 = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to8(r), to8(g), to8(b))
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

/// Pack the atlas with sprites for `realms` realms: the first realm's art as
/// drawn, and a recolored copy of every team-colored sprite for each realm
/// after it. One realm is the single-player atlas.
pub fn build(realms: u8) -> Atlas {
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
    let axeman = load(include_bytes!("../assets/textures/entities/axeman.png"));
    let enemy_axeman = load(include_bytes!(
        "../assets/textures/entities/enemy_axeman.png"
    ));
    let ally_axeman = load(include_bytes!(
        "../assets/textures/entities/ally_axeman.png"
    ));
    let axe = load(include_bytes!("../assets/textures/entities/axe.png"));
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

    // The enemy's copies of the team-colored sprites, kept aside to tell
    // which pixels carry the team color when recoloring for further realms.
    let enemy_of: HashMap<&str, RgbaImage> = HashMap::from([
        ("farmer", enemy_farmer.clone()),
        ("knight", enemy_knight.clone()),
        ("horseman", enemy_horseman.clone()),
        ("bowman", enemy_bowman.clone()),
        ("axeman", enemy_axeman.clone()),
        ("warship", enemy_warship.clone()),
        ("house", crop(&enemy_houses, 0, 0, 16, 16)),
        ("hut", enemy_hut.clone()),
    ]);

    let mut sources = vec![
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
        // Axeman sheets: 6 columns x 6 rows of 16x16 frames. Rows 0-3 walk
        // (down, up, right, left, 5 frames each); row 4 is the axe hoisted,
        // swung low and down, then the body turned right, away and left with
        // the axe out; row 5 is the same turn with the blade's trail.
        Source::sheet("axeman", axeman, 6, 6),
        Source::sheet("enemy_axeman", enemy_axeman, 6, 6),
        Source::sheet("ally_axeman", ally_axeman, 6, 6),
        // Thrown axe: 4 columns x 2 rows of 16x16 frames, the head turning a
        // full circle counterclockwise from straight up, an eighth per frame.
        Source::sheet("axe", axe, 4, 2),
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

    // Every realm after the first gets its own copy of each team-colored
    // sprite, in its own hue.
    let realms = realms.max(1);
    for realm in 1..realms {
        for base in REALM_SPRITES {
            let src = sources
                .iter()
                .find(|s| s.name == base)
                .expect("every realm sprite has a base");
            sources.push(src.for_realm(&enemy_of[base], realm));
        }
    }

    pack(sources, realms)
}

/// Simple shelf packer with 1px padding to avoid bleeding between sprites.
fn pack(mut sources: Vec<Source>, realms: u8) -> Atlas {
    const PAD: u32 = 1;
    // A single realm's sprites fit a narrow sheet; a match's recolored
    // copies need the width, or the atlas would grow taller than textures
    // may be.
    let atlas_w: u32 = if realms > 1 { 1024 } else { 512 };

    sources.sort_by_key(|s| std::cmp::Reverse(s.img.height()));

    let mut cursor_x = PAD;
    let mut cursor_y = PAD;
    let mut shelf_h = 0u32;
    let mut placements: Vec<(usize, u32, u32)> = Vec::new();

    for (i, s) in sources.iter().enumerate() {
        let (w, h) = (s.img.width(), s.img.height());
        if cursor_x + w + PAD > atlas_w {
            cursor_x = PAD;
            cursor_y += shelf_h + PAD;
            shelf_h = 0;
        }
        placements.push((i, cursor_x, cursor_y));
        cursor_x += w + PAD;
        shelf_h = shelf_h.max(h);
    }

    let atlas_h = (cursor_y + shelf_h + PAD).next_power_of_two();
    let mut atlas = RgbaImage::from_pixel(atlas_w, atlas_h, image::Rgba([0, 0, 0, 0]));
    let mut rects = HashMap::new();
    let mut sheets = HashMap::new();

    for (i, x, y) in placements {
        let src = &sources[i];
        atlas.copy_from(&src.img, x, y).unwrap();
        let (w, h) = (src.img.width(), src.img.height());
        let rect = [
            x as f32 / atlas_w as f32,
            y as f32 / atlas_h as f32,
            (x + w) as f32 / atlas_w as f32,
            (y + h) as f32 / atlas_h as f32,
        ];
        match src.grid {
            Some((cols, rows)) => {
                sheets.insert(src.name.clone(), (rect, cols, rows));
            }
            None => {
                rects.insert(src.name.clone(), rect);
            }
        }
    }

    Atlas {
        width: atlas_w,
        height: atlas_h,
        rgba: atlas.into_raw(),
        realms,
        rects,
        sheets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An atlas for a match holds a recolored copy of every team-colored
    /// sprite for each realm past the first, under the names the renderer
    /// asks for, and each is a real recolor: same shape, different hue.
    #[test]
    fn a_match_atlas_recolors_every_realm_sprite() {
        let atlas = build(3);
        assert_eq!(atlas.realms, 3);
        for realm in 1..3 {
            for base in [
                "farmer", "knight", "horseman", "bowman", "axeman", "warship",
            ] {
                let uv = atlas.frame_uv(&realm_sheet(realm, base), 0, 0);
                assert!(
                    uv[2] > uv[0] && uv[3] > uv[1],
                    "realm {realm} {base} is empty"
                );
            }
            for base in ["house", "hut"] {
                let uv = atlas.uv(&realm_sheet(realm, base));
                assert!(
                    uv[2] > uv[0] && uv[3] > uv[1],
                    "realm {realm} {base} is empty"
                );
            }
        }
        // Realms land on hues well clear of the enemy's red and the allies'
        // green, and of each other.
        let hues: Vec<f32> = (1..8).map(realm_hue).collect();
        for (i, &h) in hues.iter().enumerate() {
            let apart = |c: f32| {
                let d = (h - c).rem_euclid(360.0);
                d.min(360.0 - d)
            };
            assert!(
                apart(0.0) >= 28.0 && apart(120.0) >= 28.0,
                "realm {} hue {h}",
                i + 1
            );
            for &o in &hues[..i] {
                assert!(apart(o) >= 20.0, "realms share a hue: {h} and {o}");
            }
        }
        // The recolor leaves alpha and untouched pixels alone and changes the
        // team-colored ones.
        let blue = load(include_bytes!("../assets/textures/entities/farmer.png"));
        let red = load(include_bytes!(
            "../assets/textures/entities/enemy_farmer.png"
        ));
        let orange = recolor(&blue, &red, realm_hue(1));
        let mut changed = 0;
        for (x, y, px) in orange.enumerate_pixels() {
            let b = blue.get_pixel(x, y);
            assert_eq!(px[3], b[3], "alpha changed at {x},{y}");
            if px.0 != b.0 {
                changed += 1;
                assert_ne!(b.0[..3], red.get_pixel(x, y).0[..3]);
            }
        }
        assert!(changed > 0, "nothing was recolored");
    }

    #[test]
    fn every_hull_the_renderer_draws_is_packed() {
        // The renderer picks a ship's sheet by its owner, so each faction's
        // hull must be in the atlas or the first enemy raid would panic
        // mid-frame rather than here.
        let atlas = build(1);
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
        let atlas = build(1);
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
        let atlas = build(1);
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

    #[test]
    fn every_axeman_sheet_is_packed_with_all_six_rows() {
        // The renderer reads an axeman's frame by state and facing, so each
        // faction's sheet must be packed with its full 6x6 grid, and the
        // thrown axe with all eight turns.
        let atlas = build(1);
        for sheet in ["axeman", "enemy_axeman", "ally_axeman"] {
            for row in 0..6 {
                for col in 0..6 {
                    let uv = atlas.frame_uv(sheet, col, row);
                    assert!(
                        uv[2] > uv[0] && uv[3] > uv[1],
                        "{sheet} frame ({col}, {row}) is empty"
                    );
                }
            }
        }
        for row in 0..2 {
            for col in 0..4 {
                let uv = atlas.frame_uv("axe", col, row);
                assert!(
                    uv[2] > uv[0] && uv[3] > uv[1],
                    "axe frame ({col}, {row}) is empty"
                );
            }
        }
    }
}
