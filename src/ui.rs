//! egui integration (input side) plus the main-menu and in-game panels. The GPU
//! side (`egui_wgpu::Renderer`) lives in `gfx`.

use egui::ClippedPrimitive;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::game::{
    BuildMode, Game, GatherPriority, Priority, BRIDGE_WOOD_COST, HOUSE_STONE_COST, HOUSE_WOOD_COST,
    MINE_STONE_COST,
};

/// An action the UI is requesting the app perform this frame.
pub enum Action {
    CreateWorld(i32),
    Load,
    Resume,
    Save,
    ToMenu,
}

/// Mutable state backing the menu's widgets.
pub struct MenuState {
    pub seed_text: String,
}

impl Default for MenuState {
    fn default() -> Self {
        MenuState {
            seed_text: String::new(),
        }
    }
}

/// What the UI should draw this frame.
pub enum Scene<'a> {
    Menu {
        menu: &'a mut MenuState,
        has_save: bool,
        has_game: bool,
    },
    Game(&'a mut Game),
}

pub struct Ui {
    pub ctx: egui::Context,
    state: egui_winit::State,
}

pub struct UiOutput {
    pub primitives: Vec<ClippedPrimitive>,
    pub textures_delta: egui::TexturesDelta,
    pub pixels_per_point: f32,
}

impl Ui {
    pub fn new(window: &Window) -> Self {
        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        Ui { ctx, state }
    }

    pub fn on_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.state.on_window_event(window, event).consumed
    }

    /// Build and tessellate the UI, returning the paint data plus any action
    /// the user triggered.
    pub fn run(&mut self, window: &Window, scene: Scene) -> (UiOutput, Option<Action>) {
        let mut action = None;
        // egui's `run` takes an `FnMut` but only calls it once; move `scene`
        // through an `Option` so we can consume it inside.
        let mut scene = Some(scene);
        let raw_input = self.state.take_egui_input(window);
        let full_output = self.ctx.run(raw_input, |ctx| {
            if let Some(scene) = scene.take() {
                action = build_ui(ctx, scene);
            }
        });
        self.state
            .handle_platform_output(window, full_output.platform_output);
        let ppp = self.ctx.pixels_per_point();
        let primitives = self.ctx.tessellate(full_output.shapes, ppp);
        (
            UiOutput {
                primitives,
                textures_delta: full_output.textures_delta,
                pixels_per_point: ppp,
            },
            action,
        )
    }
}

fn build_ui(ctx: &egui::Context, scene: Scene) -> Option<Action> {
    match scene {
        Scene::Menu {
            menu,
            has_save,
            has_game,
        } => menu_ui(ctx, menu, has_save, has_game),
        Scene::Game(game) => game_ui(ctx, game),
    }
}

fn menu_ui(
    ctx: &egui::Context,
    menu: &mut MenuState,
    has_save: bool,
    has_game: bool,
) -> Option<Action> {
    let mut action = None;
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.heading("⚔  KINGDOM  ⚔");
            ui.label("Grow a kingdom on an endless procedural island.");
            ui.add_space(24.0);

            egui::Grid::new("menu_grid")
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Seed:");
                    ui.text_edit_singleline(&mut menu.seed_text);
                    if ui.button("🎲 Random").clicked() {
                        menu.seed_text = pseudo_random_seed().to_string();
                    }
                    ui.end_row();
                });

            ui.add_space(12.0);
            if ui.button("🌍  Create World").clicked() {
                let seed = parse_seed(&menu.seed_text);
                action = Some(Action::CreateWorld(seed));
            }

            ui.add_enabled_ui(has_save, |ui| {
                if ui.button("📂  Load Saved World").clicked() {
                    action = Some(Action::Load);
                }
            });

            if has_game && ui.button("▶  Resume").clicked() {
                action = Some(Action::Resume);
            }

            ui.add_space(16.0);
            ui.small("A set seed is reproducible; leave blank + Create for a random one.");
        });
    });
    action
}

fn game_ui(ctx: &egui::Context, game: &mut Game) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Kingdom")
        .default_pos([12.0, 12.0])
        .resizable(false)
        .show(ctx, |ui| {
            ui.heading("Stockpile");
            ui.label(format!("🪵 Wood: {}", game.wood));
            ui.label(format!("🪨 Stone: {}", game.stone));
            ui.separator();

            ui.label(format!(
                "Population: {} / {}",
                game.population(),
                game.pop_cap()
            ));
            ui.label(format!("Enemies defeated: {}", game.enemies_defeated));
            ui.label(format!("Units lost: {}", game.units_lost));
            ui.separator();

            ui.heading("Priority");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut game.priority, Priority::Agriculture, "🌾 Agriculture");
                ui.selectable_value(&mut game.priority, Priority::Military, "⚔ Military");
            });
            ui.horizontal(|ui| {
                ui.label("Gather:");
                ui.selectable_value(
                    &mut game.gather_priority,
                    GatherPriority::Balanced,
                    "Balanced",
                );
                ui.selectable_value(&mut game.gather_priority, GatherPriority::Wood, "🪵 Wood");
                ui.selectable_value(&mut game.gather_priority, GatherPriority::Stone, "🪨 Stone");
            });
            ui.separator();

            ui.heading("Build (left-click)");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::House,
                format!("House  ({HOUSE_WOOD_COST} wood, {HOUSE_STONE_COST} stone)"),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Bridge,
                format!("Bridge  ({BRIDGE_WOOD_COST} wood)"),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Mine,
                format!("⛏ Mine  ({MINE_STONE_COST} stone)"),
            );
            ui.radio_value(&mut game.build_mode, BuildMode::Rally, "⚑ Rally knights");
            if game.rally_point.is_some() && ui.button("✖ Clear rally").clicked() {
                game.clear_rally();
            }
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("💾 Save").clicked() {
                    action = Some(Action::Save);
                }
                if ui.button("☰ Menu").clicked() {
                    action = Some(Action::ToMenu);
                }
            });

            ui.small("Houses must be built next to your village.");
            ui.small("Houses raise new workers only while you have 4+ farmers.");
            ui.small("Farmers gather • Knights defend.");
            ui.small("Farmers replant trees and mine caves once resources run dry.");
            ui.small("Mines never run out, but only 4 farmers can work one at a time.");
            ui.small("Rally: knights march to the flag until they meet an enemy.");
            ui.small("Right-click clears the rally flag.");
            ui.small("WASD / arrows to pan • scroll to zoom.");
        });
    action
}

fn parse_seed(text: &str) -> i32 {
    let t = text.trim();
    if t.is_empty() {
        return pseudo_random_seed();
    }
    // Accept a signed integer, otherwise hash the text into a seed.
    if let Ok(v) = t.parse::<i32>() {
        v
    } else {
        let mut h: u32 = 2166136261;
        for b in t.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(16777619);
        }
        h as i32
    }
}

fn pseudo_random_seed() -> i32 {
    // Derive a seed from a monotonic clock; good enough for world variety.
    #[cfg(target_arch = "wasm32")]
    let t = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);
    #[cfg(not(target_arch = "wasm32"))]
    let t = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    };
    let bits = (t as u64).wrapping_mul(2654435761);
    (bits ^ (bits >> 32)) as i32
}
