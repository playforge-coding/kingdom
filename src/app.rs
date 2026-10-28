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
use crate::game::{
    AUTO_ZONE_RADIUS, Anim, AxeWork, CamState, Dir, Entity, Faction, Game, Job, MissileKind,
    SandboxTool, SiteKind,
};
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

/// The testing ground's tiles: light gray squares on a slightly darker gray,
/// which shows through the gap around each square as the grid's lines.
const SANDBOX_TILE: [f32; 4] = [0.74, 0.74, 0.74, 1.0];
const SANDBOX_LINE: [f32; 4] = [0.5, 0.5, 0.5, 1.0];
/// The gap left around each testing-ground tile, in tiles: the grid line.
const SANDBOX_GAP: f32 = 0.04;

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

/// Does the bridge on `(x, y)` run east to west? It does when more of its east
/// and west neighbors are deck it joins (land, or more bridge per `deck`) than
/// its north and south ones; a tie keeps the sprite's own north-south span.
fn bridge_runs_east_west(x: i32, y: i32, deck: impl Fn(i32, i32) -> bool) -> bool {
    let links = |a: (i32, i32), b: (i32, i32)| deck(a.0, a.1) as u8 + deck(b.0, b.1) as u8;
    links((x - 1, y), (x + 1, y)) > links((x, y - 1), (x, y + 1))
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
/// Seconds of play between autosaves.
const AUTOSAVE_INTERVAL: f32 = 120.0;

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
    /// The save-slot name the HUD's save box edits: prefilled from the slot a
    /// world was loaded from, or with a fresh name when one is created.
    save_name: String,
    /// Seconds of play since the last autosave.
    autosave_timer: f32,
    input: Input,
    last_time: f64,
}

impl State {
    pub async fn new(window: Arc<Window>) -> Self {
        let atlas = atlas::build();
        let renderer = Renderer::new(window.clone(), &atlas).await;
        let ui = crate::ui::Ui::new(&window);
        save::init(); // begin loading any existing saves (async on web)
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
            save_name: String::new(),
            autosave_timer: 0.0,
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

    /// Write the running world back to its own slot (the name in the HUD's
    /// save box) and start the interval over. The testing ground is never
    /// saved, and a name box emptied of anything usable skips the write.
    fn autosave(&mut self) {
        if let Some(game) = &self.game
            && !game.sandbox()
            && let Some(name) = save::sanitize_name(&self.save_name)
        {
            save::write(&name, game.to_bytes(self.cam_state()));
        }
        self.autosave_timer = 0.0;
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

        // Autosave on a timer that only runs while actually playing.
        self.autosave_timer += dt;
        if self.autosave_timer >= AUTOSAVE_INTERVAL {
            self.autosave();
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
            if game.sandbox() {
                game.sandbox_click(world);
            } else {
                game.try_build(world);
            }
        }
    }

    /// The tile under the cursor, for the testing ground's placement cursor.
    fn cursor_tile(&self) -> (i32, i32) {
        let world = self.camera.screen_to_world(
            self.input.cursor.0 as f32,
            self.input.cursor.1 as f32,
            self.renderer.size.width as f32,
            self.renderer.size.height as f32,
        );
        (world.x.floor() as i32, world.y.floor() as i32)
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
                self.save_name = format!("world {seed}");
                self.autosave_timer = 0.0;
                log::info!("created world with seed {seed}");
            }
            Action::CreateSandbox => {
                self.game = Some(Game::new_sandbox());
                self.camera.center = Vec2::ZERO;
                self.camera.view_height = DEFAULT_VIEW_HEIGHT;
                self.scene = Scene::Playing;
                log::info!("opened the testing ground");
            }
            Action::Load(name) => {
                if let Some(bytes) = save::read(&name) {
                    if let Some((game, cam)) = Game::from_bytes(&bytes) {
                        self.camera.center = Vec2::new(cam.cx, cam.cy);
                        self.camera.view_height = cam.view_height;
                        self.game = Some(game);
                        self.scene = Scene::Playing;
                        self.save_name = name.clone();
                        self.autosave_timer = 0.0;
                        log::info!("loaded saved world '{name}'");
                    } else {
                        log::warn!("save '{name}' could not be parsed");
                    }
                }
            }
            Action::Resume => {
                if self.game.is_some() {
                    self.scene = Scene::Playing;
                }
            }
            Action::Save(name) => {
                // The testing ground is never saved: a slot holding an empty
                // grid would only mislead.
                if let Some(game) = &self.game
                    && !game.sandbox()
                    && let Some(name) = save::sanitize_name(&name)
                {
                    save::write(&name, game.to_bytes(self.cam_state()));
                    // Keep the save box showing the name actually stored.
                    self.save_name = name;
                }
            }
            Action::DeleteSave(name) => {
                save::delete(&name);
            }
            Action::ToMenu => {
                // Leaving for the menu is a natural stopping point, and often
                // the way out of the game entirely, so take a snapshot.
                self.autosave();
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

    /// Assemble the draw list for the visible tile rectangle: terrain and
    /// buildings/nodes, then ships, then bridges (so hulls pass under them),
    /// then animated entities (and their HP bars) on top.
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
        let bridge_ew = self.atlas.uv("bridge_ew");
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
        // Bridge decks go down after the ships, so a hull sails under a span
        // instead of over it.
        let mut bridges = Vec::new();

        let tinted = |pos: [f32; 2], size: [f32; 2], uv: [f32; 4], color: [f32; 4]| Instance {
            pos,
            size,
            uv_min: [uv[0], uv[1]],
            uv_max: [uv[2], uv[3]],
            color,
        };
        let quad = move |pos: [f32; 2], size: [f32; 2], uv: [f32; 4]| tinted(pos, size, uv, WHITE);

        // The testing ground is a plain gray grid: one darker sheet under the
        // whole view, and a lighter square inset on every grass tile so the
        // sheet shows through the gaps as the grid's lines. Water painted on
        // it keeps its real texture, as do the structures drawn below.
        let sandbox = game.sandbox();
        if sandbox {
            out.push(tinted(
                [minx as f32, miny as f32],
                [(maxx - minx + 1) as f32, (maxy - miny + 1) as f32],
                white,
                SANDBOX_LINE,
            ));
        }

        for y in miny..=maxy {
            for x in minx..=maxx {
                let p = [x as f32, y as f32];
                if sandbox && world.tile(x, y) == Tile::Grass {
                    out.push(tinted(
                        [x as f32 + SANDBOX_GAP, y as f32 + SANDBOX_GAP],
                        [1.0 - 2.0 * SANDBOX_GAP, 1.0 - 2.0 * SANDBOX_GAP],
                        white,
                        SANDBOX_TILE,
                    ));
                } else {
                    let uv = match world.tile(x, y) {
                        Tile::Water => water,
                        Tile::Grass => grass,
                    };
                    out.push(quad(p, [1.0, 1.0], uv));
                }

                if world.is_bridge(x, y) {
                    bridges.push((x, y));
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

        // Bridge decks, each turned to run along its crossing: east to west
        // when it joins deck on those sides, north to south otherwise. Bridges
        // still being built count as deck, so a span ordered tile by tile
        // lines up before it's finished.
        let pending: Vec<_> = game
            .build_sites()
            .iter()
            .filter(|s| s.kind == SiteKind::Bridge)
            .map(|s| s.tile)
            .collect();
        let deck = |x: i32, y: i32| {
            world.tile(x, y) == Tile::Grass || world.is_bridge(x, y) || pending.contains(&(x, y))
        };
        let bridge_uv = |x: i32, y: i32| {
            if bridge_runs_east_west(x, y, deck) {
                bridge_ew
            } else {
                bridge
            }
        };
        for &(x, y) in &bridges {
            out.push(quad([x as f32, y as f32], [1.0, 1.0], bridge_uv(x, y)));
        }

        // The testing ground's placement cursor: the tile under the pointer,
        // washed in the color of the faction about to own what goes down
        // there, or plain white for terrain that belongs to no one.
        let owned = match game.sandbox_tool {
            SandboxTool::PlaceUnit => Some(true),
            SandboxTool::PlaceTile => Some(game.place_brush.owned()),
            SandboxTool::Rally => None,
        };
        if sandbox && let Some(owned) = owned {
            let (cx, cy) = self.cursor_tile();
            let mut wash = match game.place_faction {
                _ if !owned => WHITE,
                Faction::Player => self.colors.player,
                Faction::Enemy => self.colors.enemy,
                Faction::Ally => self.colors.ally,
            };
            wash[3] = 0.45;
            out.push(tinted([cx as f32, cy as f32], [1.0, 1.0], white, wash));
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

        // Cannonballs in flight: a small sprite centered on the shot.
        let cannonball = self.atlas.uv("cannonball");
        for b in game.cannonballs() {
            out.push(quad([b.pos.x - 0.2, b.pos.y - 0.2], [0.4, 0.4], cannonball));
        }

        // Arrows in flight: the shaft drawn along its line of flight. The sheet
        // holds one frame pointing down and one pointing left; the other two
        // headings mirror those by swapping the UV corners. A thrown axe
        // tumbles instead, turning over toward the way it flies.
        for a in game.missiles() {
            if a.kind == MissileKind::Axe {
                let turn = (a.age * AXE_SPIN_FPS) as u32 % 8;
                // The sheet turns counterclockwise; an axe flying right turns
                // the other way, head over toward its heading.
                let f = if a.vel.x > 0.0 { (8 - turn) % 8 } else { turn };
                let uv = self.atlas.frame_uv("axe", f % 4, f / 4);
                out.push(quad([a.pos.x - 0.35, a.pos.y - 0.35], [0.7, 0.7], uv));
                continue;
            }
            let mut uv = if a.vel.x.abs() >= a.vel.y.abs() {
                self.atlas.frame_uv("arrow", 0, 1)
            } else {
                self.atlas.frame_uv("arrow", 0, 0)
            };
            if a.vel.x.abs() >= a.vel.y.abs() {
                if a.vel.x > 0.0 {
                    uv.swap(0, 2);
                }
            } else if a.vel.y < 0.0 {
                uv.swap(1, 3);
            }
            out.push(quad([a.pos.x - 0.3, a.pos.y - 0.3], [0.6, 0.6], uv));
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
                SiteKind::Bridge => bridge_uv(s.tile.0, s.tile.1),
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

        // Rally flags: the knights' waypoints, drawn on top of everything.
        for r in game.rallies() {
            let rally = self.atlas.uv("rally");
            out.push(quad([r.pos.x - 0.5, r.pos.y - 0.5], [1.0, 1.0], rally));
        }

        // Navy rally flags: the warships' waypoints, tinted the player's blue
        // to tell them apart from the knights' flags.
        for r in game.navy_rallies() {
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
                    // The testing ground is gray out to infinity, so nothing
                    // on it is "unexplored".
                    if game.sandbox() {
                        SANDBOX_TILE
                    } else {
                        c.unexplored
                    }
                } else {
                    match world.tile(sx, sy) {
                        Tile::Water if world.is_bridge(sx, sy) => c.rock,
                        Tile::Water => c.water,
                        Tile::Grass => match world.node(sx, sy).map(|n| n.kind) {
                            Some(Resource::Wood) => c.tree,
                            Some(Resource::Stone) => c.rock,
                            None if game.sandbox() => SANDBOX_TILE,
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

        for r in game.rallies() {
            let size = 1.5 * kf;
            out.push(sprite(
                [r.pos.x - size * 0.5, r.pos.y - size * 0.5],
                [size, size],
                self.atlas.uv("rally"),
            ));
        }

        // The navy's flags keep their player-blue tint out here too.
        for r in game.navy_rallies() {
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
                        save_name: &mut self.save_name,
                    },
                ),
                None => self.ui.run(
                    window,
                    UiScene::Menu {
                        menu: &mut self.menu,
                        slots: save::slots(),
                        has_game: false,
                    },
                ),
            },
            Scene::Menu => {
                let has_game = self.game.is_some();
                self.ui.run(
                    window,
                    UiScene::Menu {
                        menu: &mut self.menu,
                        slots: save::slots(),
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

/// Per-sheet animation layout for the foot sheets, 5-column grids of 16x16
/// frames. Walk animations live on rows `walk_base..+4` and actions on
/// `act_base..+4`, one row per facing in the order down, up, then the two
/// sides (see `sprite_frame` for which side comes first).
struct AnimRows {
    walk_base: u32,
    walk_frames: u32,
    act_base: u32,
    act_frames: u32,
    /// The action block's two side rows come in the opposite order to the
    /// walk block's.
    act_sides_flipped: bool,
}

// Farmer: walk rows 0-3, chop/mine rows 8-11 (3 frames).
const FARMER_ROWS: AnimRows = AnimRows {
    walk_base: 0,
    walk_frames: 5,
    act_base: 8,
    act_frames: 3,
    act_sides_flipped: false,
};
// Knight: walk rows 0-3, sword-swing rows 4-7 (4 frames).
const KNIGHT_ROWS: AnimRows = AnimRows {
    walk_base: 0,
    walk_frames: 5,
    act_base: 4,
    act_frames: 4,
    act_sides_flipped: false,
};
// Bowman: walk rows 0-3, bow-draw rows 4-7 (3 frames), on an 8-row sheet.
// The walk rows run in the knight's side order, the draw rows the other way.
const BOWMAN_ROWS: AnimRows = AnimRows {
    walk_base: 0,
    walk_frames: 5,
    act_base: 4,
    act_frames: 3,
    act_sides_flipped: true,
};

/// Frames per second every walk and action cycle runs at.
const ANIM_FPS: f32 = 8.0;
/// Frames per second an axeman's spin turns through: a full turn every four
/// frames, so it whirls rather than plods.
const SPIN_FPS: f32 = 14.0;
/// Frames per second a thrown axe tumbles through its eight turns.
const AXE_SPIN_FPS: f32 = 20.0;

/// Map an entity's state + facing to a (sheet, column, row) frame.
fn sprite_frame(e: &Entity) -> (&'static str, u32, u32) {
    // Civilians keep the classic farmer art whatever job they've taken up.
    let sheet = match (e.faction, e.job) {
        (Faction::Player, Job::Civilian) => "farmer",
        (Faction::Player, Job::Knight) => "knight",
        (Faction::Player, Job::Horseman) => "horseman",
        (Faction::Player, Job::Bowman) => "bowman",
        (Faction::Player, Job::Axeman) => "axeman",
        (Faction::Enemy, Job::Civilian) => "enemy_farmer",
        (Faction::Enemy, Job::Knight) => "enemy_knight",
        (Faction::Enemy, Job::Horseman) => "enemy_horseman",
        (Faction::Enemy, Job::Bowman) => "enemy_bowman",
        (Faction::Enemy, Job::Axeman) => "enemy_axeman",
        (Faction::Ally, Job::Civilian) => "ally_farmer",
        (Faction::Ally, Job::Knight) => "ally_knight",
        (Faction::Ally, Job::Horseman) => "ally_horseman",
        (Faction::Ally, Job::Bowman) => "ally_bowman",
        (Faction::Ally, Job::Axeman) => "ally_axeman",
    };
    let rows = match e.job {
        Job::Civilian => FARMER_ROWS,
        Job::Knight => KNIGHT_ROWS,
        Job::Bowman => BOWMAN_ROWS,
        Job::Horseman => {
            let (col, row) = horseman_frame(e);
            return (sheet, col, row);
        }
        Job::Axeman => {
            let (col, row) = axeman_frame(e);
            return (sheet, col, row);
        }
    };

    let (base, frames) = match e.anim {
        Anim::Idle => (rows.walk_base, 1),
        Anim::Walk => (rows.walk_base, rows.walk_frames),
        Anim::Act => (rows.act_base, rows.act_frames),
    };

    // Row order within a state block: down, up, then the two side rows. The
    // side art is mirrored from what the raw row order implies, so Right uses
    // the third row and Left uses the fourth, except on a sheet whose action
    // block has the sides the other way around.
    let flipped = matches!(e.anim, Anim::Act) && rows.act_sides_flipped;
    let dir_offset = match (e.facing, flipped) {
        (Dir::Down, _) => 0,
        (Dir::Up, _) => 1,
        (Dir::Right, false) | (Dir::Left, true) => 2,
        (Dir::Left, false) | (Dir::Right, true) => 3,
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

/// The axeman sheets are 6x6 grids of 16x16 frames: four walk rows (down,
/// up, right, left, 5 frames), then a row of the axe at work (hoisted, swung
/// low, swung down, then the body turned right, away and left with the axe
/// out) and the same row again with the blade's trail. A windup holds the
/// axe hoisted; the spin swings it low and then turns round and round with
/// the trail; a throw shows the arm out toward the facing; any other work
/// (felling, hacking at a wall, bridging) chops hoist, low, down. Returns the
/// (column, row) of the frame to draw.
fn axeman_frame(e: &Entity) -> (u32, u32) {
    match e.axe_work() {
        Some(AxeWork::Windup) => return (0, 4),
        Some(AxeWork::Spin(t)) => {
            let f = (t * SPIN_FPS) as u32;
            return (if f == 0 { 1 } else { 2 + (f - 1) % 4 }, 5);
        }
        Some(AxeWork::Throw) => {
            let col = match e.facing {
                Dir::Down => 2,
                Dir::Right => 3,
                Dir::Up => 4,
                Dir::Left => 5,
            };
            return (col, 4);
        }
        None => {}
    }
    let row = match e.facing {
        Dir::Down => 0,
        Dir::Up => 1,
        Dir::Right => 2,
        Dir::Left => 3,
    };
    let tick = (e.anim_time * ANIM_FPS) as u32;
    match e.anim {
        Anim::Idle => (0, row),
        Anim::Walk => (tick % 5, row),
        Anim::Act => (tick % 3, 4),
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
            WindowEvent::CloseRequested => {
                // One last snapshot so closing the window never loses more
                // than the current autosave interval.
                state.autosave();
                event_loop.exit();
            }
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
                // On the testing ground a right-click clears the tile of
                // whoever stands on it, or of whatever was painted there; in
                // a real game it lifts the flags.
                if let Some(game) = &mut state.game {
                    if game.sandbox() {
                        let world = state.camera.screen_to_world(
                            state.input.cursor.0 as f32,
                            state.input.cursor.1 as f32,
                            state.renderer.size.width as f32,
                            state.renderer.size.height as f32,
                        );
                        game.sandbox_erase(world);
                    } else {
                        game.clear_rally();
                        game.clear_navy_rally();
                    }
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
                            // Leaving play is a way out of the session, so
                            // autosave; the world can't change on the menu,
                            // so quitting from there has nothing new to save.
                            if state.scene == Scene::Playing {
                                state.autosave();
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
