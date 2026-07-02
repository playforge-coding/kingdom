//! Window/event-loop glue: owns the renderer + game, translates input, and
//! assembles the per-frame instance list. Uses the winit 0.30
//! `ApplicationHandler` pattern with an async-initialised `State` delivered back
//! through an `EventLoopProxy` (needed because device creation is async on web).

use std::sync::Arc;

use glam::Vec2;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::atlas::{self, Atlas};
use crate::camera::Camera;
use crate::game::{Anim, CamState, Dir, Entity, Faction, Game, Job};
use crate::gfx::{Instance, Renderer};
use crate::save;
use crate::ui::{Action, MenuState, Scene as UiScene};
use crate::world::{Resource, Tile};

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    Menu,
    Playing,
}

const DEFAULT_VIEW_HEIGHT: f32 = 28.0;
/// Tiles beyond the visible rectangle that are still simulated, so units walking
/// on/off screen behave seamlessly.
const SIM_MARGIN: i32 = 24;
/// Most same-type units drawn on a single tile; caps overdraw from huge stacks
/// without hiding ordinary crowds.
const STACK_DRAW_CAP: u32 = 8;

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
    cursor: (f64, f64),
}

pub struct State {
    window: Arc<Window>,
    renderer: Renderer,
    atlas: Atlas,
    camera: Camera,
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
        camera.aspect = renderer.size.width as f32 / renderer.size.height.max(1) as f32;
        State {
            window,
            renderer,
            atlas,
            camera,
            ui,
            scene: Scene::Menu,
            menu: MenuState::default(),
            game: None,
            input: Input::default(),
            last_time: now_secs(),
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        self.renderer.resize(w, h);
        self.camera.aspect = w as f32 / h.max(1) as f32;
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

        // Simulate a generous rectangle around the view; entities outside it are
        // frozen so unwatched crowds don't cost anything.
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
        }
    }

    /// Assemble the draw list for the visible tile rectangle: terrain, bridges,
    /// buildings/nodes, then animated entities (and their HP bars) on top.
    fn build_instances(&self) -> Vec<Instance> {
        let Some(game) = &self.game else {
            return Vec::new();
        };
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
        // how many same-type units are drawn on any one tile — small crowds show
        // fully (so you see newcomers arrive), but a pathological pile-up of
        // hundreds costs a handful of quads instead of hundreds.
        let mut stack: std::collections::HashMap<(i32, i32, bool, bool), u32> =
            std::collections::HashMap::new();
        for e in &game.entities {
            let key = (
                e.pos.x.floor() as i32,
                e.pos.y.floor() as i32,
                matches!(e.faction, Faction::Player),
                matches!(e.job, Job::Knight),
            );
            let count = stack.entry(key).or_insert(0);
            *count += 1;
            if *count > STACK_DRAW_CAP {
                continue;
            }
            let (sheet, col, row) = sprite_frame(e);
            let uv = self.atlas.frame_uv(sheet, col, row);
            out.push(tinted(
                [e.pos.x - 0.5, e.pos.y - 0.5],
                [1.0, 1.0],
                uv,
                WHITE,
            ));

            if e.hp < e.max_hp {
                let ratio = (e.hp / e.max_hp).clamp(0.0, 1.0);
                let (bw, bh) = (0.8f32, 0.12f32);
                let bx = e.pos.x - bw / 2.0;
                let by = e.pos.y - 0.72;
                out.push(tinted([bx, by], [bw, bh], white, [0.15, 0.0, 0.0, 0.85]));
                let col = match e.faction {
                    Faction::Player => [0.3, 0.55, 1.0, 0.95],
                    Faction::Ally => [0.25, 0.85, 0.3, 0.95],
                    Faction::Enemy => [0.9, 0.25, 0.2, 0.95],
                };
                out.push(tinted([bx, by], [bw * ratio, bh], white, col));
            }
        }

        // Cargo ships at sea: a wide sprite with a gentle 3-frame bob. Each
        // 32x32 frame covers a 2x2-tile footprint, drawn centred on the hull.
        for s in game.ships() {
            let col = ((s.bob * 3.0) as u32) % 3;
            let uv = self.atlas.frame_uv("cargo_ship", col, 0);
            out.push(quad([s.pos.x - 1.0, s.pos.y - 1.0], [2.0, 2.0], uv));
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
        }

        // Cannonballs in flight: a small sprite centred on the shot.
        let cannonball = self.atlas.uv("cannonball");
        for b in game.cannonballs() {
            out.push(quad([b.pos.x - 0.2, b.pos.y - 0.2], [0.4, 0.4], cannonball));
        }

        // Pending hut orders: a ghostly hut on each tree awaiting a builder.
        for &(ox, oy) in game.hut_orders() {
            out.push(tinted(
                [ox as f32, oy as f32],
                [1.0, 1.0],
                hut,
                [1.0, 1.0, 1.0, 0.4],
            ));
        }

        // Rally flag: the player's knight waypoint, drawn on top of everything.
        if let Some(r) = game.rally_point {
            let rally = self.atlas.uv("rally");
            out.push(quad([r.x - 0.5, r.y - 0.5], [1.0, 1.0], rally));
        }

        out
    }

    fn render(&mut self) {
        // Ensure visible chunks are generated before we read them.
        if self.scene == Scene::Playing {
            let (minx, miny, maxx, maxy) = self.visible_bounds();
            if let Some(game) = &mut self.game {
                game.world.ensure_region(minx, miny, maxx, maxy);
            }
        }

        self.renderer.set_camera(self.camera.view_proj());
        let instances = self.build_instances();

        // Build the egui frame (disjoint field borrows on self).
        let window = &self.window;
        let (mut ui_out, action) = match self.scene {
            Scene::Playing => match &mut self.game {
                Some(game) => self.ui.run(window, UiScene::Game(game)),
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

        match self.renderer.render(&instances, &mut ui_out) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let (w, h) = (self.renderer.size.width, self.renderer.size.height);
                self.renderer.resize(w, h);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                log::error!("surface out of memory");
            }
            Err(e) => log::warn!("surface error: {e:?}"),
        }

        if let Some(action) = action {
            self.apply_action(action);
        }
    }
}

/// Per-sheet animation layout. The sheets are 5x12 grids of 16x16 frames.
/// Walk animations live on rows `walk_base..+4` and actions on
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

/// Map an entity's state + facing to a (sheet, column, row) frame.
fn sprite_frame(e: &Entity) -> (&'static str, u32, u32) {
    let sheet = match (e.faction, e.job) {
        (Faction::Player, Job::Farmer) => "farmer",
        (Faction::Player, Job::Knight) => "knight",
        (Faction::Enemy, Job::Farmer) => "enemy_farmer",
        (Faction::Enemy, Job::Knight) => "enemy_knight",
        (Faction::Ally, Job::Farmer) => "ally_farmer",
        (Faction::Ally, Job::Knight) => "ally_knight",
    };
    let rows = match e.job {
        Job::Farmer => FARMER_ROWS,
        Job::Knight => KNIGHT_ROWS,
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
        ((e.anim_time * 8.0) as u32) % frames
    };
    (sheet, col, row)
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

    // Delivery of the async-initialised State (web path).
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, state: State) {
        state.window.request_redraw();
        self.state = Some(state);
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
            WindowEvent::MouseWheel { delta, .. } if !ui_consumed => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                // Scroll up -> zoom in.
                state.camera.zoom(if scroll > 0.0 { 0.9 } else { 1.1 });
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
                state.render();
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
