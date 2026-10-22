//! Window/event-loop glue: owns the renderer + game, translates input, and
//! assembles the per-frame instance list. Uses the winit 0.30
//! `ApplicationHandler` pattern with an async-initialized `State` delivered back
//! through an `EventLoopProxy` (needed because device creation is async on web).

use std::sync::Arc;

use glam::Vec2;
use rustc_hash::FxHashMap as HashMap;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::atlas::{self, Atlas, MapColors};
use crate::camera::{Camera, DETAIL_VIEW_HEIGHT};
use crate::game::{AUTO_ZONE_RADIUS, Anim, CamState, Dir, Entity, Faction, Game, Job, SiteKind};
use crate::gfx::{Instance, Renderer};
use crate::save;
use crate::ui::{Action, MenuState, Scene as UiScene, View};
use crate::world::{Resource, Tile};

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// A mild player-blue wash over the cargo-ship sprite that marks a settler
/// ship, so an expedition reads apart from the trade fleet at a glance.
const SETTLER_TINT: [f32; 4] = [0.72, 0.82, 1.0, 1.0];
/// The deeper blue wash that marks a troop ship: still the player's color, but
/// unmistakably the military run rather than the colonists'.
const TROOP_TINT: [f32; 4] = [0.45, 0.6, 1.0, 1.0];

/// The allied green that marks a hired company's landing banner: the allies'
/// own color, since the mercenaries aboard remain their knights.
const MERC_TINT: [f32; 4] = [0.6, 1.0, 0.65, 1.0];
/// The enemy red that marks a sea raid's landing banner, so the coast it's
/// bound for reads as a threat at a glance.
const RAID_TINT: [f32; 4] = [1.0, 0.5, 0.45, 1.0];

/// The sprite sheet a transport's hull is drawn from. The player's missions
/// sail the cargo hull in a tint (see `transport_tint`); an allied company
/// and an enemy raid sail their own faction's hull, drawn as is.
fn transport_sheet(t: &crate::game::Transport) -> &'static str {
    match t.faction {
        crate::game::Faction::Player => "cargo_ship",
        crate::game::Faction::Enemy => "enemy_warship",
        crate::game::Faction::Ally => "ally_warship",
    }
}

/// The hull tint for a transport: a blue wash on the player's cargo hull
/// (mild for settlers, deeper for troops), none on a faction's own hull.
fn transport_tint(t: &crate::game::Transport) -> [f32; 4] {
    match t.faction {
        crate::game::Faction::Player if t.knights > 0 => TROOP_TINT,
        crate::game::Faction::Player => SETTLER_TINT,
        crate::game::Faction::Enemy | crate::game::Faction::Ally => WHITE,
    }
}

/// The tint of the banner planted on a transport's landing: the mission's
/// blue for the player's, the allies' green for a hired company, the enemy's
/// red for a raid.
fn banner_tint(t: &crate::game::Transport) -> [f32; 4] {
    match t.faction {
        crate::game::Faction::Player => transport_tint(t),
        crate::game::Faction::Enemy => RAID_TINT,
        crate::game::Faction::Ally => MERC_TINT,
    }
}

/// The sprite sheet a warship of `faction` is drawn from.
fn warship_sheet(faction: crate::game::Faction) -> &'static str {
    match faction {
        crate::game::Faction::Player => "warship",
        crate::game::Faction::Enemy => "enemy_warship",
        crate::game::Faction::Ally => "ally_warship",
    }
}

/// The fill of a warship's hull bar, in its faction's color.
fn hull_bar_tint(faction: crate::game::Faction) -> [f32; 4] {
    match faction {
        crate::game::Faction::Player => [0.3, 0.55, 1.0, 0.95],
        crate::game::Faction::Enemy => [0.9, 0.25, 0.2, 0.95],
        crate::game::Faction::Ally => [0.35, 0.85, 0.4, 0.95],
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    Menu,
    Playing,
}

const DEFAULT_VIEW_HEIGHT: f32 = 28.0;
/// How fast a held zoom key zooms, as a natural-log change in view height per
/// second (about 1.5 s to cross the whole zoom range).
const ZOOM_RATE: f32 = 1.5;
/// Tiles beyond the visible rectangle that still run at the full frame rate,
/// so units walking on/off screen behave seamlessly.
const SIM_MARGIN: i32 = 24;
/// Most same-type units drawn on a single tile; caps overdraw from huge stacks
/// without hiding ordinary crowds.
const STACK_DRAW_CAP: u32 = 8;
/// Zoom the overview toggle (M) jumps out to.
const OVERVIEW_VIEW_HEIGHT: f32 = 240.0;
/// Most chunks the overview generates per frame. A far zoom can uncover
/// hundreds at once, so they fill in over a few frames rather than in one stall.
const OVERVIEW_CHUNK_BUDGET: usize = 6;

fn now_secs() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now() / 1000.0)
            .unwrap_or(0.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64()
    }
}

#[derive(Default)]
struct Input {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    zoom_in: bool,
    zoom_out: bool,
    cursor: (f64, f64),
}

pub struct State {
    window: Arc<Window>,
    renderer: Renderer,
    atlas: Atlas,
    colors: MapColors,
    camera: Camera,
    /// The detailed zoom to return to when the overview toggle is pressed again.
    detail_view_height: f32,
    ui: crate::ui::Ui,
    scene: Scene,
    menu: MenuState,
    game: Option<Game>,
    input: Input,
    last_time: f64,
}

impl State {
    pub async fn new(window: Arc<Window>) -> Self {
        let atlas = atlas::build();
        let renderer = Renderer::new(window.clone(), &atlas).await;
        let ui = crate::ui::Ui::new(&window);
        save::init(); // begin loading any existing save (async on web)
        let mut camera = Camera::new(Vec2::ZERO);
        camera.aspect =
            (renderer.size.width as f32 / renderer.size.height.max(1) as f32).clamp(0.1, 10.0);
        State {
            window,
            renderer,
            colors: MapColors::from_atlas(&atlas),
            atlas,
            camera,
            detail_view_height: DEFAULT_VIEW_HEIGHT,
            ui,
            scene: Scene::Menu,
            menu: MenuState::default(),
            game: None,
            input: Input::default(),
            last_time: now_secs(),
        }
    }

    /// Track a window resize. macOS delivers degenerate (zero) sizes while a
    /// window is dragged between displays or minimized; adopting one would
    /// blow the camera's aspect up to the window's whole width, and the aspect
    /// scales the tile rectangle that terrain generation and the draw list
    /// cover every frame, so those sizes are ignored (and the aspect clamped
    /// well past any real monitor, as a backstop).
    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.renderer.resize(w, h);
        self.camera.aspect = (w as f32 / h as f32).clamp(0.1, 10.0);
    }

    fn cam_state(&self) -> CamState {
        CamState {
            cx: self.camera.center.x,
            cy: self.camera.center.y,
            view_height: self.camera.view_height,
        }
    }

    fn update(&mut self) {
        let t = now_secs();
        let dt = ((t - self.last_time) as f32).max(0.0);
        self.last_time = t;

        if self.scene != Scene::Playing {
            return;
        }

        // Pan the camera with WASD / arrows, scaled by zoom.
        let pan = self.camera.view_height * 0.9 * dt;
        let mut d = Vec2::ZERO;
        if self.input.up {
            d.y -= 1.0;
        }
        if self.input.down {
            d.y += 1.0;
        }
        if self.input.left {
            d.x -= 1.0;
        }
        if self.input.right {
            d.x += 1.0;
        }
        if d != Vec2::ZERO {
            self.camera.center += d.normalize() * pan;
        }

        // Zoom with dedicated keys rather than the scroll wheel, which
        // free-spins on smooth-scrolling mice. Exponential so it feels even
        // at every zoom level.
        let mut z = 0.0;
        if self.input.zoom_in {
            z -= 1.0;
        }
        if self.input.zoom_out {
            z += 1.0;
        }
        if z != 0.0 {
            self.camera.zoom((z * ZOOM_RATE * dt).exp());
        }

        // Entities in a generous rectangle around the view run at the full
        // frame rate; the rest of the realm is stepped in slower, rotating
        // batches (see `Game::update`).
        let (minx, miny, maxx, maxy) = self.visible_bounds();
        let sim = (
            minx - SIM_MARGIN,
            miny - SIM_MARGIN,
            maxx + SIM_MARGIN,
            maxy + SIM_MARGIN,
        );
        if let Some(game) = &mut self.game {
            game.update(dt, sim);
        }
    }

    fn build_at_cursor(&mut self) {
        let world = self.camera.screen_to_world(
            self.input.cursor.0 as f32,
            self.input.cursor.1 as f32,
            self.renderer.size.width as f32,
            self.renderer.size.height as f32,
        );
        if let Some(game) = &mut self.game {
            game.try_build(world);
        }
    }

    /// Visible tile rectangle (inclusive) with a small margin.
    fn visible_bounds(&self) -> (i32, i32, i32, i32) {
        let hh = self.camera.view_height * 0.5;
        let hw = hh * self.camera.aspect;
        let c = self.camera.center;
        (
            (c.x - hw).floor() as i32 - 1,
            (c.y - hh).floor() as i32 - 1,
            (c.x + hw).ceil() as i32 + 1,
            (c.y + hh).ceil() as i32 + 1,
        )
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::CreateWorld(seed) => {
                let game = Game::new(seed);
                self.camera.center = game.start_center();
                self.game = Some(game);
                self.camera.view_height = DEFAULT_VIEW_HEIGHT;
                self.scene = Scene::Playing;
                log::info!("created world with seed {seed}");
            }
            Action::Load => {
                if let Some(bytes) = save::read() {
                    if let Some((game, cam)) = Game::from_bytes(&bytes) {
                        self.camera.center = Vec2::new(cam.cx, cam.cy);
                        self.camera.view_height = cam.view_height;
                        self.game = Some(game);
                        self.scene = Scene::Playing;
                        log::info!("loaded saved world");
                    } else {
                        log::warn!("save file could not be parsed");
                    }
                }
            }
            Action::Resume => {
                if self.game.is_some() {
                    self.scene = Scene::Playing;
                }
            }
            Action::Save => {
                if let Some(game) = &self.game {
                    save::write(game.to_bytes(self.cam_state()));
                }
            }
            Action::ToMenu => {
                self.scene = Scene::Menu;
            }
            Action::Focus { pos, zoom_in } => {
                self.camera.center = pos;
                if zoom_in && self.camera.overview() {
                    self.camera.view_height = self.detail_view_height;
                }
            }
            Action::ToggleOverview => self.toggle_overview(),
        }
    }

    /// Jump between the current detailed zoom and the flat overview.
    fn toggle_overview(&mut self) {
        if self.camera.overview() {
            self.camera.view_height = self.detail_view_height;
        } else {
            self.detail_view_height = self.camera.view_height;
            self.camera.view_height = OVERVIEW_VIEW_HEIGHT;
        }
    }

    /// Assemble the draw list for the visible tile rectangle: terrain, bridges,
    /// buildings/nodes, then animated entities (and their HP bars) on top.
    fn build_instances(&self) -> Vec<Instance> {
        let Some(game) = &self.game else {
            return Vec::new();
        };
        if self.camera.overview() {
            return self.build_overview_instances(game);
        }
        let world = &game.world;
        let grass = self.atlas.uv("grass");
        let water = self.atlas.uv("water");
        let bridge = self.atlas.uv("bridge");
        let tree = self.atlas.uv("tree");
        let rock = self.atlas.uv("rock");
        let house = self.atlas.uv("house");
        let enemy_house = self.atlas.uv("enemy_house");
        let ally_house = self.atlas.uv("ally_house");
        let wall = self.atlas.uv("wall");
        let cave = self.atlas.uv("cave");
        let hut = self.atlas.uv("hut");
        let enemy_hut = self.atlas.uv("enemy_hut");
        let ally_hut = self.atlas.uv("ally_hut");
        let white = self.atlas.uv("white");

        let (minx, miny, maxx, maxy) = self.visible_bounds();
        let cap = ((maxx - minx + 1) * (maxy - miny + 1)) as usize * 2 + 256;
        let mut out = Vec::with_capacity(cap);

        let tinted = |pos: [f32; 2], size: [f32; 2], uv: [f32; 4], color: [f32; 4]| Instance {
            pos,
            size,
            uv_min: [uv[0], uv[1]],
            uv_max: [uv[2], uv[3]],
            color,
        };
        let quad = move |pos: [f32; 2], size: [f32; 2], uv: [f32; 4]| tinted(pos, size, uv, WHITE);

        for y in miny..=maxy {
            for x in minx..=maxx {
                let p = [x as f32, y as f32];
                let uv = match world.tile(x, y) {
                    Tile::Water => water,
                    Tile::Grass => grass,
                };
                out.push(quad(p, [1.0, 1.0], uv));

                if world.is_bridge(x, y) {
                    out.push(quad(p, [1.0, 1.0], bridge));
                }
                if world.is_house(x, y) {
                    out.push(quad(p, [1.0, 1.0], house));
                } else if world.is_enemy_house(x, y) {
                    out.push(quad(p, [1.0, 1.0], enemy_house));
                } else if world.is_ally_house(x, y) {
                    out.push(quad(p, [1.0, 1.0], ally_house));
                } else if world.is_cave(x, y) {
                    out.push(quad(p, [1.0, 1.0], cave));
                } else if let Some(h) = world.hut(x, y) {
                    let uv = match h.owner {
                        0 => hut,
                        2 => ally_hut,
                        _ => enemy_hut,
                    };
                    out.push(quad(p, [1.0, 1.0], uv));
                } else if let Some(w) = world.wall(x, y) {
                    // Tint walls by faction so they read apart: enemy red, ally green.
                    let tint = match w.owner {
                        0 => WHITE,
                        2 => [0.55, 1.0, 0.6, 1.0],
                        _ => [1.0, 0.55, 0.5, 1.0],
                    };
                    out.push(tinted(p, [1.0, 1.0], wall, tint));
                } else if let Some(node) = world.node(x, y) {
                    let uv = match node.kind {
                        Resource::Wood => tree,
                        Resource::Stone => rock,
                    };
                    out.push(quad(p, [1.0, 1.0], uv));
                }
            }
        }

        // Saplings: a young tree that scales up from a sprout as it matures.
        for (sx, sy, grow) in game.saplings_iter() {
            let s = 0.3 + 0.7 * grow.clamp(0.0, 1.0);
            let px = sx as f32 + (1.0 - s) * 0.5;
            let py = sy as f32 + (1.0 - s);
            out.push(quad([px, py], [s, s], tree));
        }

        // Entities: pick an animation frame and draw an HP bar when hurt. We cap
        // how many same-type units are drawn on any one tile. Small crowds show
        // fully (so you see newcomers arrive), but a pathological pile-up of
        // hundreds costs a handful of quads instead of hundreds.
        let mut stack: HashMap<(i32, i32, bool, Job), u32> = HashMap::default();
        for e in &game.entities {
            let key = (
                e.pos.x.floor() as i32,
                e.pos.y.floor() as i32,
                matches!(e.faction, Faction::Player),
                e.job,
            );
            let count = stack.entry(key).or_insert(0);
            *count += 1;
            if *count > STACK_DRAW_CAP {
                continue;
            }
            let (sheet, col, row) = sprite_frame(e);
            let uv = self.atlas.frame_uv(sheet, col, row);
            // A horseman's frames are 32x32, drawn over a 2x2-tile footprint
            // with the hooves on its tile, and lifted off the ground while it
            // leaps a stream. Everyone else is a 16x16 frame on one tile.
            let mounted = e.job == Job::Horseman;
            let lift = e.jump_lift();
            if mounted {
                out.push(tinted(
                    [e.pos.x - 1.0, e.pos.y - HORSEMAN_DRAW_RISE - lift],
                    [2.0, 2.0],
                    uv,
                    WHITE,
                ));
            } else {
                out.push(tinted(
                    [e.pos.x - 0.5, e.pos.y - 0.5],
                    [1.0, 1.0],
                    uv,
                    WHITE,
                ));
            }

            if e.hp < e.max_hp {
                let ratio = (e.hp / e.max_hp).clamp(0.0, 1.0);
                let (bw, bh) = (0.8f32, 0.12f32);
                let bx = e.pos.x - bw / 2.0;
                let by = if mounted {
                    e.pos.y - HORSEMAN_DRAW_RISE - lift + 0.05
                } else {
                    e.pos.y - 0.72
                };
                out.push(tinted([bx, by], [bw, bh], white, [0.15, 0.0, 0.0, 0.85]));
                let col = match e.faction {
                    Faction::Player => [0.3, 0.55, 1.0, 0.95],
                    Faction::Ally => [0.25, 0.85, 0.3, 0.95],
                    Faction::Enemy => [0.9, 0.25, 0.2, 0.95],
                };
                out.push(tinted([bx, by], [bw * ratio, bh], white, col));
            }
        }

        // Cargo ships at sea: a 32x32 directional sprite (columns face left, up,
        // down, right), drawn over a 2x2-tile footprint, centered on the hull.
        for s in game.ships() {
            let col = match s.facing {
                Dir::Left => 0,
                Dir::Up => 1,
                Dir::Down => 2,
                Dir::Right => 3,
            };
            let uv = self.atlas.frame_uv("cargo_ship", col, 0);
            out.push(quad([s.pos.x - 1.0, s.pos.y - 1.0], [2.0, 2.0], uv));
        }

        // Transports, drawn over the same 2x2-tile footprint: the player's
        // sail the cargo hull in a blue wash (mild for settlers, deeper for
        // troops), an allied company and an enemy raid their own hull.
        for s in game.transports() {
            let col = match s.facing {
                Dir::Left => 0,
                Dir::Up => 1,
                Dir::Down => 2,
                Dir::Right => 3,
            };
            let uv = self.atlas.frame_uv(transport_sheet(s), col, 0);
            out.push(tinted(
                [s.pos.x - 1.0, s.pos.y - 1.0],
                [2.0, 2.0],
                uv,
                transport_tint(s),
            ));
        }

        // Warships of every flag: a 32x32 directional sprite (columns face
        // left, up, down, right) from the owner's sheet, drawn over a 2x2-tile
        // footprint, with an HP bar in the owner's color when the hull is
        // damaged.
        for w in game.warships() {
            let col = match w.facing {
                Dir::Left => 0,
                Dir::Up => 1,
                Dir::Down => 2,
                Dir::Right => 3,
            };
            let uv = self.atlas.frame_uv(warship_sheet(w.faction), col, 0);
            out.push(quad([w.pos.x - 1.0, w.pos.y - 1.0], [2.0, 2.0], uv));
            let ratio = w.hp_ratio();
            if ratio < 1.0 {
                let (bw, bh) = (1.2f32, 0.16f32);
                let bx = w.pos.x - bw / 2.0;
                let by = w.pos.y - 1.05;
                out.push(tinted([bx, by], [bw, bh], white, [0.15, 0.0, 0.0, 0.85]));
                out.push(tinted(
                    [bx, by],
                    [bw * ratio, bh],
                    white,
                    hull_bar_tint(w.faction),
                ));
            }
        }

        // Pirate ships: a 32x32 directional sprite (columns face left, up, down,
        // right), drawn over a 2x2-tile footprint.
        for p in game.pirates() {
            let col = match p.facing {
                Dir::Left => 0,
                Dir::Up => 1,
                Dir::Down => 2,
                Dir::Right => 3,
            };
            let uv = self.atlas.frame_uv("pirate_ship", col, 0);
            out.push(quad([p.pos.x - 1.0, p.pos.y - 1.0], [2.0, 2.0], uv));
            let ratio = p.hp_ratio();
            if ratio < 1.0 {
                let (bw, bh) = (1.2f32, 0.16f32);
                let bx = p.pos.x - bw / 2.0;
                let by = p.pos.y - 1.05;
                out.push(tinted([bx, by], [bw, bh], white, [0.15, 0.0, 0.0, 0.85]));
                out.push(tinted(
                    [bx, by],
                    [bw * ratio, bh],
                    white,
                    [0.9, 0.25, 0.2, 0.95],
                ));
            }
        }

        // Cannonballs in flight: a small sprite centered on the shot.
        let cannonball = self.atlas.uv("cannonball");
        for b in game.cannonballs() {
            out.push(quad([b.pos.x - 0.2, b.pos.y - 0.2], [0.4, 0.4], cannonball));
        }

        // Builder zones: a translucent plot over each marked area, brighter
        // while the autonomous-building switch is on, with a small center mark.
        let zone_span = (AUTO_ZONE_RADIUS * 2 + 1) as f32;
        for &(zx, zy) in game.auto_zones() {
            let mut fill = self.colors.player;
            fill[3] = if game.auto_build { 0.16 } else { 0.08 };
            out.push(tinted(
                [
                    (zx - AUTO_ZONE_RADIUS) as f32,
                    (zy - AUTO_ZONE_RADIUS) as f32,
                ],
                [zone_span, zone_span],
                white,
                fill,
            ));
            let mut mark = self.colors.player;
            mark[3] = 0.6;
            out.push(tinted(
                [zx as f32 + 0.25, zy as f32 + 0.25],
                [0.5, 0.5],
                white,
                mark,
            ));
        }

        // Construction sites: a ghost of the coming structure that solidifies
        // as its builder works.
        for s in game.build_sites() {
            let uv = match s.kind {
                SiteKind::House => house,
                SiteKind::Bridge => bridge,
                SiteKind::Mine => cave,
                SiteKind::Wall => wall,
                SiteKind::Hut => hut,
            };
            let alpha = 0.35 + 0.5 * s.progress();
            out.push(tinted(
                [s.tile.0 as f32, s.tile.1 as f32],
                [1.0, 1.0],
                uv,
                [1.0, 1.0, 1.0, alpha],
            ));
        }

        // Rally flag: the player's knight waypoint, drawn on top of everything.
        if let Some(r) = game.rally_point {
            let rally = self.atlas.uv("rally");
            out.push(quad([r.x - 0.5, r.y - 0.5], [1.0, 1.0], rally));
        }

        // Navy rally flag: the warships' waypoint, tinted the player's blue to
        // tell it apart from the knights' flag.
        if let Some(r) = game.navy_rally {
            let rally = self.atlas.uv("rally");
            let mut tint = self.colors.player;
            tint[3] = 1.0;
            out.push(tinted([r.x - 0.5, r.y - 0.5], [1.0, 1.0], rally, tint));
        }

        // Each voyage plants a banner on its landing the moment it sets sail,
        // so the click that sent it gets an answer on the spot and the
        // destination stays marked until the ship arrives, in its owner's
        // tint. An enemy raid's red banner is the warning of where it will
        // come ashore.
        for s in game.transports() {
            let rally = self.atlas.uv("rally");
            let (ax, ay) = s.anchor;
            out.push(tinted(
                [ax as f32, ay as f32],
                [1.0, 1.0],
                rally,
                banner_tint(s),
            ));
        }

        out
    }

    /// The far-zoom draw list. Past `DETAIL_VIEW_HEIGHT` a tile is only a pixel
    /// or two, so terrain is drawn as flat blocks of several tiles each, with
    /// each village's territory, houses and units as solid marks on top. This
    /// keeps the instance count flat however far out the camera goes.
    fn build_overview_instances(&self, game: &Game) -> Vec<Instance> {
        let world = &game.world;
        let c = &self.colors;
        let white = self.atlas.uv("white");
        let solid = |pos: [f32; 2], size: [f32; 2], color: [f32; 4]| Instance {
            pos,
            size,
            uv_min: [white[0], white[1]],
            uv_max: [white[2], white[3]],
            color,
        };
        let sprite = |pos: [f32; 2], size: [f32; 2], uv: [f32; 4]| Instance {
            pos,
            size,
            uv_min: [uv[0], uv[1]],
            uv_max: [uv[2], uv[3]],
            color: WHITE,
        };
        let faction_color = |f: Faction| match f {
            Faction::Player => c.player,
            Faction::Enemy => c.enemy,
            Faction::Ally => c.ally,
        };
        let with_alpha = |mut col: [f32; 4], a: f32| {
            col[3] = a;
            col
        };

        // Block size in tiles, chosen so about `DETAIL_VIEW_HEIGHT` blocks span
        // the view's height. Blocks snap to a fixed grid so panning doesn't
        // make the terrain shimmer.
        let k = (self.camera.view_height / DETAIL_VIEW_HEIGHT)
            .ceil()
            .max(1.0) as i32;
        let kf = k as f32;
        let (minx, miny, maxx, maxy) = self.visible_bounds();
        let (bx0, by0) = (minx.div_euclid(k) * k, miny.div_euclid(k) * k);
        let cols = (maxx - bx0) / k + 1;
        let rows = (maxy - by0) / k + 1;
        let mut out = Vec::with_capacity((cols * rows) as usize + 512);

        for by in (by0..=maxy).step_by(k as usize) {
            for bx in (bx0..=maxx).step_by(k as usize) {
                let (sx, sy) = (bx + k / 2, by + k / 2);
                let color = if !world.is_generated(sx, sy) {
                    c.unexplored
                } else {
                    match world.tile(sx, sy) {
                        Tile::Water if world.is_bridge(sx, sy) => c.rock,
                        Tile::Water => c.water,
                        Tile::Grass => match world.node(sx, sy).map(|n| n.kind) {
                            Some(Resource::Wood) => c.tree,
                            Some(Resource::Stone) => c.rock,
                            None => c.grass,
                        },
                    }
                };
                out.push(solid([bx as f32, by as f32], [kf, kf], color));
            }
        }

        // Territory: a tinted box around each village with a border in its
        // faction's color. A player village under attack flashes its border
        // red so trouble is visible from across the map.
        let pulse = (now_secs() * 4.0).sin() as f32 * 0.5 + 0.5;
        let border = 0.4 * kf;
        for v in game.villages() {
            let pad = 3.0;
            let x0 = v.min.0 as f32 - pad;
            let y0 = v.min.1 as f32 - pad;
            let w = (v.max.0 - v.min.0) as f32 + 1.0 + 2.0 * pad;
            let h = (v.max.1 - v.min.1) as f32 + 1.0 + 2.0 * pad;
            let base = faction_color(v.faction);
            out.push(solid([x0, y0], [w, h], with_alpha(base, 0.22)));
            let edge = if v.faction == Faction::Player && v.threatened {
                let mut e = c.enemy;
                for (ch, b) in e.iter_mut().zip(base) {
                    *ch = b + (*ch - b) * pulse;
                }
                e
            } else {
                base
            };
            out.push(solid([x0, y0], [w, border], edge));
            out.push(solid([x0, y0 + h - border], [w, border], edge));
            out.push(solid([x0, y0], [border, h], edge));
            out.push(solid([x0 + w - border, y0], [border, h], edge));
        }

        // Houses and huts as solid marks a little bigger than their tile so
        // they stay visible when a tile is only a pixel or two.
        let mark = (0.6 * kf).max(1.0);
        let mut house = |(x, y): (i32, i32), col: [f32; 4], size: f32| {
            let off = (size - 1.0) * 0.5;
            out.push(solid([x as f32 - off, y as f32 - off], [size, size], col));
        };
        for &t in game.player_houses() {
            house(t, c.player, mark);
        }
        for &t in &world.enemy_house_tiles {
            house(t, c.enemy, mark);
        }
        for &t in &world.ally_house_tiles {
            house(t, c.ally, mark);
        }
        for &(x, y) in game.huts() {
            if let Some(h) = world.hut(x, y) {
                let col = match h.owner {
                    0 => c.player,
                    2 => c.ally,
                    _ => c.enemy,
                };
                house((x, y), col, mark * 0.75);
            }
        }

        // Builder zones as faint plots, so the overview shows where autonomous
        // building is permitted.
        let zone_span = (AUTO_ZONE_RADIUS * 2 + 1) as f32;
        for &(zx, zy) in game.auto_zones() {
            let mut fill = c.player;
            fill[3] = if game.auto_build { 0.18 } else { 0.1 };
            out.push(solid(
                [
                    (zx - AUTO_ZONE_RADIUS) as f32,
                    (zy - AUTO_ZONE_RADIUS) as f32,
                ],
                [zone_span, zone_span],
                fill,
            ));
        }

        // Units as dots: knights in the full faction color, civilians lighter.
        let dot = (0.45 * kf).max(0.8);
        for e in &game.entities {
            let mut col = faction_color(e.faction);
            if e.job == Job::Civilian {
                for ch in col.iter_mut().take(3) {
                    *ch += (1.0 - *ch) * 0.45;
                }
            }
            out.push(solid(
                [e.pos.x - dot * 0.5, e.pos.y - dot * 0.5],
                [dot, dot],
                col,
            ));
        }

        // Ships keep their sprites, scaled up so they read at this distance.
        let ship = (1.2 * kf).max(2.0);
        let facing_col = |d: Dir| match d {
            Dir::Left => 0,
            Dir::Up => 1,
            Dir::Down => 2,
            Dir::Right => 3,
        };
        let mut vessel = |pos: Vec2, sheet: &str, d: Dir| {
            let uv = self.atlas.frame_uv(sheet, facing_col(d), 0);
            out.push(sprite(
                [pos.x - ship * 0.5, pos.y - ship * 0.5],
                [ship, ship],
                uv,
            ));
        };
        for s in game.ships() {
            vessel(s.pos, "cargo_ship", s.facing);
        }
        for w in game.warships() {
            vessel(w.pos, warship_sheet(w.faction), w.facing);
        }
        for p in game.pirates() {
            vessel(p.pos, "pirate_ship", p.facing);
        }
        // Transports keep their hull and tint out here too.
        for s in game.transports() {
            let uv = self
                .atlas
                .frame_uv(transport_sheet(s), facing_col(s.facing), 0);
            out.push(Instance {
                pos: [s.pos.x - ship * 0.5, s.pos.y - ship * 0.5],
                size: [ship, ship],
                uv_min: [uv[0], uv[1]],
                uv_max: [uv[2], uv[3]],
                color: transport_tint(s),
            });
        }

        if let Some(r) = game.rally_point {
            let size = 1.5 * kf;
            out.push(sprite(
                [r.x - size * 0.5, r.y - size * 0.5],
                [size, size],
                self.atlas.uv("rally"),
            ));
        }

        // The navy's flag keeps its player-blue tint out here too.
        if let Some(r) = game.navy_rally {
            let size = 1.5 * kf;
            let uv = self.atlas.uv("rally");
            let mut tint = c.player;
            tint[3] = 1.0;
            out.push(Instance {
                pos: [r.x - size * 0.5, r.y - size * 0.5],
                size: [size, size],
                uv_min: [uv[0], uv[1]],
                uv_max: [uv[2], uv[3]],
                color: tint,
            });
        }

        // So does each voyage's landing banner, which matters most at this
        // zoom: the whole crossing fits on screen, ship and destination, and
        // an enemy raid's red banner shows which coast it's bound for.
        for s in game.transports() {
            let size = 1.5 * kf;
            let uv = self.atlas.uv("rally");
            let (ax, ay) = s.anchor;
            out.push(Instance {
                pos: [ax as f32 + 0.5 - size * 0.5, ay as f32 + 0.5 - size * 0.5],
                size: [size, size],
                uv_min: [uv[0], uv[1]],
                uv_max: [uv[2], uv[3]],
                color: banner_tint(s),
            });
        }

        out
    }

    /// Build and draw one frame. Returns whether it was actually presented
    /// (false while the surface can't hand out textures, e.g. mid window-drag
    /// between displays), so the event loop can pace itself in the meantime.
    fn render(&mut self) -> bool {
        // Ensure visible chunks are generated before we read them.
        if self.scene == Scene::Playing {
            let (minx, miny, maxx, maxy) = self.visible_bounds();
            if let Some(game) = &mut self.game {
                if self.camera.overview() {
                    game.world
                        .ensure_region_budget(minx, miny, maxx, maxy, OVERVIEW_CHUNK_BUDGET);
                } else {
                    game.world.ensure_region(minx, miny, maxx, maxy);
                }
            }
        }

        self.renderer.set_camera(self.camera.view_proj());
        let instances = self.build_instances();

        // Build the egui frame (disjoint field borrows on self).
        let window = &self.window;
        let view = View {
            center: self.camera.center,
            half: self.camera.half_size(),
            overview: self.camera.overview(),
        };
        let (mut ui_out, action) = match self.scene {
            Scene::Playing => match &mut self.game {
                Some(game) => self.ui.run(
                    window,
                    UiScene::Game {
                        game,
                        view,
                        colors: &self.colors,
                    },
                ),
                None => {
                    let has_save = save::exists();
                    self.ui.run(
                        window,
                        UiScene::Menu {
                            menu: &mut self.menu,
                            has_save,
                            has_game: false,
                        },
                    )
                }
            },
            Scene::Menu => {
                let has_save = save::exists();
                let has_game = self.game.is_some();
                self.ui.run(
                    window,
                    UiScene::Menu {
                        menu: &mut self.menu,
                        has_save,
                        has_game,
                    },
                )
            }
        };

        let presented = self.renderer.render(&instances, &mut ui_out);

        if let Some(action) = action {
            self.apply_action(action);
        }
        presented
    }
}

/// How far above its position's center a horseman's 2x2-tile quad starts, in
/// tiles: places the hooves on the unit's tile with the rider rising above it.
const HORSEMAN_DRAW_RISE: f32 = 1.4;

/// Per-sheet animation layout for the foot sheets, 5x12 grids of 16x16
/// frames. Walk animations live on rows `walk_base..+4` and actions on
/// `act_base..+4`, one row per facing in the order down, up, left, right.
struct AnimRows {
    walk_base: u32,
    walk_frames: u32,
    act_base: u32,
    act_frames: u32,
}

// Farmer: walk rows 0-3, chop/mine rows 8-11 (3 frames).
const FARMER_ROWS: AnimRows = AnimRows {
    walk_base: 0,
    walk_frames: 5,
    act_base: 8,
    act_frames: 3,
};
// Knight: walk rows 0-3, sword-swing rows 4-7 (4 frames).
const KNIGHT_ROWS: AnimRows = AnimRows {
    walk_base: 0,
    walk_frames: 5,
    act_base: 4,
    act_frames: 4,
};

/// Frames per second every walk and action cycle runs at.
const ANIM_FPS: f32 = 8.0;

/// Map an entity's state + facing to a (sheet, column, row) frame.
fn sprite_frame(e: &Entity) -> (&'static str, u32, u32) {
    // Civilians keep the classic farmer art whatever job they've taken up.
    let sheet = match (e.faction, e.job) {
        (Faction::Player, Job::Civilian) => "farmer",
        (Faction::Player, Job::Knight) => "knight",
        (Faction::Player, Job::Horseman) => "horseman",
        (Faction::Enemy, Job::Civilian) => "enemy_farmer",
        (Faction::Enemy, Job::Knight) => "enemy_knight",
        (Faction::Enemy, Job::Horseman) => "enemy_horseman",
        (Faction::Ally, Job::Civilian) => "ally_farmer",
        (Faction::Ally, Job::Knight) => "ally_knight",
        (Faction::Ally, Job::Horseman) => "ally_horseman",
    };
    let rows = match e.job {
        Job::Civilian => FARMER_ROWS,
        Job::Knight => KNIGHT_ROWS,
        Job::Horseman => {
            let (col, row) = horseman_frame(e);
            return (sheet, col, row);
        }
    };

    // Row order within a state block: down, up, then the two side rows. The
    // side art is mirrored from what the raw row order implies, so Right uses
    // the third row and Left uses the fourth.
    let dir_offset = match e.facing {
        Dir::Down => 0,
        Dir::Up => 1,
        Dir::Right => 2,
        Dir::Left => 3,
    };

    let (base, frames) = match e.anim {
        Anim::Idle => (rows.walk_base, 1),
        Anim::Walk => (rows.walk_base, rows.walk_frames),
        Anim::Act => (rows.act_base, rows.act_frames),
    };
    let row = base + dir_offset;
    let col = if frames <= 1 {
        0
    } else {
        ((e.anim_time * ANIM_FPS) as u32) % frames
    };
    (sheet, col, row)
}

/// The horseman sheets are 4x12 grids of 32x32 frames laid out as a block of
/// three rows per facing, in the order down, right, left, up: two rows of
/// riding (one 8-frame cycle read across both rows) and then one row of the
/// lance thrust (4 frames). Returns the (column, row) of the frame to draw.
fn horseman_frame(e: &Entity) -> (u32, u32) {
    let block = match e.facing {
        Dir::Down => 0,
        Dir::Right => 1,
        Dir::Left => 2,
        Dir::Up => 3,
    } * 3;
    match e.anim {
        Anim::Idle => (0, block),
        Anim::Walk => {
            let f = ((e.anim_time * ANIM_FPS) as u32) % 8;
            (f % 4, block + f / 4)
        }
        Anim::Act => (((e.anim_time * ANIM_FPS) as u32) % 4, block + 2),
    }
}

pub struct App {
    state: Option<State>,
    // Only consumed on the wasm path (async State delivery).
    #[allow(dead_code)]
    proxy: Option<EventLoopProxy<State>>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<State>) -> Self {
        App {
            state: None,
            proxy: Some(proxy),
        }
    }
}

impl ApplicationHandler<State> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes().with_title("Kingdom");

        #[cfg(target_arch = "wasm32")]
        {
            use winit::platform::web::WindowAttributesExtWebSys;
            // Size to the browser viewport.
            let (w, h) = web_sys::window()
                .map(|win| {
                    (
                        win.inner_width()
                            .ok()
                            .and_then(|v| v.as_f64())
                            .unwrap_or(960.0),
                        win.inner_height()
                            .ok()
                            .and_then(|v| v.as_f64())
                            .unwrap_or(600.0),
                    )
                })
                .unwrap_or((960.0, 600.0));
            attrs = attrs
                .with_inner_size(winit::dpi::LogicalSize::new(w, h))
                .with_append(true);
        }

        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        #[cfg(not(target_arch = "wasm32"))]
        {
            self.state = Some(pollster::block_on(State::new(window)));
        }

        #[cfg(target_arch = "wasm32")]
        {
            if let Some(proxy) = self.proxy.take() {
                wasm_bindgen_futures::spawn_local(async move {
                    let state = State::new(window).await;
                    let _ = proxy.send_event(state);
                });
            }
        }
    }

    // Delivery of the async-initialized State (web path).
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, state: State) {
        state.window.request_redraw();
        let state = self.state.insert(state);
        // Any Resized events that fired during async init were dropped (no
        // state to receive them), and the renderer may have captured the
        // canvas before layout, so sync to the real size now.
        let size = state.window.inner_size();
        state.resize(size.width.max(1), size.height.max(1));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        // Let egui see the event first; if it consumed a pointer interaction we
        // don't also act on it in the game world.
        let ui_consumed = state.ui.on_event(&state.window, &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.resize(size.width, size.height),
            WindowEvent::CursorMoved { position, .. } => {
                state.input.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !ui_consumed => state.build_at_cursor(),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if !ui_consumed => {
                if let Some(game) = &mut state.game {
                    game.clear_rally();
                    game.clear_navy_rally();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::KeyW | KeyCode::ArrowUp => state.input.up = pressed,
                        KeyCode::KeyS | KeyCode::ArrowDown => state.input.down = pressed,
                        KeyCode::KeyA | KeyCode::ArrowLeft => state.input.left = pressed,
                        KeyCode::KeyD | KeyCode::ArrowRight => state.input.right = pressed,
                        KeyCode::KeyE | KeyCode::Equal | KeyCode::NumpadAdd => {
                            state.input.zoom_in = pressed
                        }
                        KeyCode::KeyQ | KeyCode::Minus | KeyCode::NumpadSubtract => {
                            state.input.zoom_out = pressed
                        }
                        KeyCode::KeyM
                            if pressed && !event.repeat && state.scene == Scene::Playing =>
                        {
                            state.toggle_overview()
                        }
                        KeyCode::Escape if pressed => {
                            // Playing -> back to menu; on the menu -> quit.
                            if state.scene == Scene::Playing {
                                state.scene = Scene::Menu;
                            } else {
                                event_loop.exit();
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                state.update();
                #[allow(unused_variables)]
                let presented = state.render();
                // Presenting (vsync) is the only thing pacing the Poll +
                // request_redraw loop. While frames are being skipped, rest a
                // moment instead of spinning flat out; on the web the browser
                // paces redraws itself.
                #[cfg(not(target_arch = "wasm32"))]
                if !presented {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }
}

pub fn run() {
    let event_loop = winit::event_loop::EventLoop::<State>::with_user_event()
        .build()
        .unwrap();
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    let proxy = event_loop.create_proxy();
    let app = App::new(proxy);
    log::info!("Kingdom starting. Create or load a world from the menu.");

    #[cfg(target_arch = "wasm32")]
    {
        use winit::platform::web::EventLoopExtWebSys;
        event_loop.spawn_app(app);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut app = app;
        event_loop.run_app(&mut app).unwrap();
    }
}
