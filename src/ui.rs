//! egui integration (input side) plus the main-menu and in-game panels. The GPU
//! side (`egui_wgpu::Renderer`) lives in `gfx`.

use egui::{ClippedPrimitive, Image, ImageSource, RichText, WidgetAtom};
use winit::event::WindowEvent;
use winit::window::Window;

use crate::game::{
    BRIDGE_WOOD_COST, BuildMode, Game, GatherPriority, HOUSE_GOLD_COST, HOUSE_STONE_COST,
    HOUSE_WOOD_COST, HUT_GOLD_COST, KNIGHT_GOLD_COST, MINE_GOLD_COST, MINE_STONE_COST, Priority,
    STONE_PRICE, WALL_GOLD_COST, WALL_STONE_COST, WALL_WOOD_COST, WARSHIP_GOLD_COST,
    WARSHIP_STONE_COST, WARSHIP_WOOD_COST, WOOD_PRICE,
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
#[derive(Default)]
pub struct MenuState {
    pub seed_text: String,
}

/// HUD icons: Lucide SVGs (see `assets/icons/LICENSE`) drawn with white
/// strokes, so `icon` can tint them to match the text around them.
mod icons {
    use egui::{ImageSource, include_image};

    pub const ANCHOR: ImageSource = include_image!("../assets/icons/anchor.svg");
    pub const BRICK_WALL: ImageSource = include_image!("../assets/icons/brick-wall.svg");
    pub const COINS: ImageSource = include_image!("../assets/icons/coins.svg");
    pub const DICES: ImageSource = include_image!("../assets/icons/dices.svg");
    pub const FLAG: ImageSource = include_image!("../assets/icons/flag.svg");
    pub const FOLDER_OPEN: ImageSource = include_image!("../assets/icons/folder-open.svg");
    pub const GLOBE: ImageSource = include_image!("../assets/icons/globe.svg");
    pub const MENU: ImageSource = include_image!("../assets/icons/menu.svg");
    pub const MINUS: ImageSource = include_image!("../assets/icons/minus.svg");
    pub const MOUNTAIN: ImageSource = include_image!("../assets/icons/mountain.svg");
    pub const PICKAXE: ImageSource = include_image!("../assets/icons/pickaxe.svg");
    pub const PLAY: ImageSource = include_image!("../assets/icons/play.svg");
    pub const PLUS: ImageSource = include_image!("../assets/icons/plus.svg");
    pub const SAVE: ImageSource = include_image!("../assets/icons/save.svg");
    pub const SCROLL_TEXT: ImageSource = include_image!("../assets/icons/scroll-text.svg");
    pub const SHIP: ImageSource = include_image!("../assets/icons/ship.svg");
    pub const SWORDS: ImageSource = include_image!("../assets/icons/swords.svg");
    pub const TENT: ImageSource = include_image!("../assets/icons/tent.svg");
    pub const TREE_PINE: ImageSource = include_image!("../assets/icons/tree-pine.svg");
    pub const WHEAT: ImageSource = include_image!("../assets/icons/wheat.svg");
    pub const X: ImageSource = include_image!("../assets/icons/x.svg");
}

/// An icon sized to the body text and tinted to the current text color.
fn icon<'a>(ui: &egui::Ui, source: ImageSource<'a>) -> Image<'a> {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    Image::new(source)
        .fit_to_exact_size(egui::vec2(size, size))
        .tint(ui.visuals().text_color())
}

/// A non-interactive label with a leading icon.
fn icon_label(ui: &mut egui::Ui, source: ImageSource, text: impl Into<egui::WidgetText>) {
    ui.add(WidgetAtom::new((icon(ui, source), text.into())));
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
        egui_extras::install_image_loaders(&ctx);
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
    pub fn run(&mut self, window: &Window, mut scene: Scene) -> (UiOutput, Option<Action>) {
        let mut action = None;
        // egui may run the closure more than once in a frame (it discards a
        // pass when layout settles late, e.g. while an icon loads), so each
        // pass borrows `scene` again and the last one to ask for an action wins.
        let raw_input = self.state.take_egui_input(window);
        let full_output = self.ctx.run_ui(raw_input, |ui| {
            if let Some(a) = build_ui(ui, &mut scene) {
                action = Some(a);
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

fn build_ui(ui: &mut egui::Ui, scene: &mut Scene) -> Option<Action> {
    match scene {
        Scene::Menu {
            menu,
            has_save,
            has_game,
        } => menu_ui(ui, menu, *has_save, *has_game),
        Scene::Game(game) => game_ui(ui.ctx(), game),
    }
}

fn menu_ui(
    ui: &mut egui::Ui,
    menu: &mut MenuState,
    has_save: bool,
    has_game: bool,
) -> Option<Action> {
    let mut action = None;
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            let swords = icon(ui, icons::SWORDS).fit_to_exact_size(egui::vec2(20.0, 20.0));
            ui.add(WidgetAtom::new((
                swords.clone(),
                RichText::new("KINGDOM").heading(),
                swords,
            )));
            ui.label("Grow a kingdom on an endless procedural island.");
            ui.add_space(24.0);

            egui::Grid::new("menu_grid")
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Seed:");
                    ui.text_edit_singleline(&mut menu.seed_text);
                    if ui.button((icon(ui, icons::DICES), "Random")).clicked() {
                        menu.seed_text = pseudo_random_seed().to_string();
                    }
                    ui.end_row();
                });

            ui.add_space(12.0);
            if ui
                .button((icon(ui, icons::GLOBE), "Create World"))
                .clicked()
            {
                let seed = parse_seed(&menu.seed_text);
                action = Some(Action::CreateWorld(seed));
            }

            ui.add_enabled_ui(has_save, |ui| {
                if ui
                    .button((icon(ui, icons::FOLDER_OPEN), "Load Saved World"))
                    .clicked()
                {
                    action = Some(Action::Load);
                }
            });

            if has_game && ui.button((icon(ui, icons::PLAY), "Resume")).clicked() {
                action = Some(Action::Resume);
            }

            ui.add_space(16.0);
            ui.small("A set seed is reproducible; leave it blank for a random one.");
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
            icon_label(ui, icons::TREE_PINE, format!("Wood: {}", game.wood));
            icon_label(ui, icons::MOUNTAIN, format!("Stone: {}", game.stone));
            icon_label(ui, icons::COINS, format!("Gold: {}", game.money));
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
                ui.selectable_value(&mut game.priority, Priority::Agriculture,
                    (icon(ui, icons::WHEAT), "Agriculture"),
                );
                ui.selectable_value(&mut game.priority, Priority::Military,
                    (icon(ui, icons::SWORDS), "Military"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Gather:");
                ui.selectable_value(
                    &mut game.gather_priority,
                    GatherPriority::Balanced,
                    "Balanced",
                );
                ui.selectable_value(&mut game.gather_priority, GatherPriority::Wood,
                    (icon(ui, icons::TREE_PINE), "Wood"),
                );
                ui.selectable_value(&mut game.gather_priority, GatherPriority::Stone,
                    (icon(ui, icons::MOUNTAIN), "Stone"),
                );
            });
            ui.separator();

            ui.heading("Proclamations");
            if let Some(secs) = game.draft_remaining() {
                icon_label(
                    ui,
                    icons::SCROLL_TEXT,
                    format!("Draft in force, {:.0}s left", secs.ceil()),
                );
            } else if ui
                .button((icon(ui, icons::SCROLL_TEXT), "Proclaim draft"))
                .clicked()
            {
                game.proclaim_draft();
            }
            ui.small(format!(
                "Draft: farmers may take up arms as knights ({KNIGHT_GOLD_COST} gold each)."
            ));
            ui.separator();

            ui.heading("Build (left-click)");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::House,
                format!("House  ({HOUSE_WOOD_COST} wood, {HOUSE_STONE_COST} stone, {HOUSE_GOLD_COST} gold)"),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Bridge,
                format!("Bridge  ({BRIDGE_WOOD_COST} wood)"),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Mine,
                (
                    icon(ui, icons::PICKAXE),
                    format!("Mine  ({MINE_STONE_COST} stone, {MINE_GOLD_COST} gold)"),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Wall,
                (
                    icon(ui, icons::BRICK_WALL),
                    format!("Wall  ({WALL_WOOD_COST} wood, {WALL_STONE_COST} stone, {WALL_GOLD_COST} gold)"),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Hut,
                (
                    icon(ui, icons::TENT),
                    format!("Hut  (click a tree; {HUT_GOLD_COST} gold)"),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Rally,
                (icon(ui, icons::FLAG), "Rally knights"),
            );
            if game.rally_point.is_some()
                && ui.button((icon(ui, icons::X), "Clear rally")).clicked()
            {
                game.clear_rally();
            }
            ui.separator();

            ui.heading("Trade");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Ship,
                (
                    icon(ui, icons::SHIP),
                    "Ship goods (open water by your village)",
                ),
            );
            // Adjust the cargo the next ship carries. Load is capped to the
            // stockpile when the ship is actually dispatched.
            ui.horizontal(|ui| {
                icon_label(ui, icons::TREE_PINE, format!("Wood: {}", game.ship_wood));
                if ui.small_button(icon(ui, icons::MINUS)).clicked() {
                    game.ship_wood = game.ship_wood.saturating_sub(5);
                }
                if ui.small_button(icon(ui, icons::PLUS)).clicked() {
                    game.ship_wood += 5;
                }
            });
            ui.horizontal(|ui| {
                icon_label(ui, icons::MOUNTAIN, format!("Stone: {}", game.ship_stone));
                if ui.small_button(icon(ui, icons::MINUS)).clicked() {
                    game.ship_stone = game.ship_stone.saturating_sub(5);
                }
                if ui.small_button(icon(ui, icons::PLUS)).clicked() {
                    game.ship_stone += 5;
                }
            });
            icon_label(
                ui,
                icons::COINS,
                format!(
                    "Payout: {} gold ({WOOD_PRICE} per wood, {STONE_PRICE} per stone)",
                    game.ship_payout()
                ),
            );
            ui.small("Ships sail to the nearest allied coast, banking gold on arrival.");
            ui.separator();

            ui.heading("Navy");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Warship,
                (
                    icon(ui, icons::ANCHOR),
                    format!(
                        "Warship  ({WARSHIP_WOOD_COST} wood, {WARSHIP_STONE_COST} stone, {WARSHIP_GOLD_COST} gold)"
                    ),
                ),
            );
            ui.small("Launch on open water by your village; it hunts pirates and shells enemies ashore.");
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button((icon(ui, icons::SAVE), "Save")).clicked() {
                    action = Some(Action::Save);
                }
                if ui.button((icon(ui, icons::MENU), "Menu")).clicked() {
                    action = Some(Action::ToMenu);
                }
            });

            ui.small("Houses must be built next to your village.");
            ui.small("Houses raise new workers only while you have 4 or more farmers.");
            ui.small(format!(
                "Each new knight costs {KNIGHT_GOLD_COST} gold; a broke village raises a farmer instead."
            ));
            ui.small("Trade: load a ship on the coast and it sails to an allied shore for gold.");
            ui.small("Allies fight the enemy on their own, but never join your battles.");
            ui.small("Farmers gather and knights defend.");
            ui.small("Farmers replant trees and mine caves once resources run dry.");
            ui.small("Mines never run out, but only 4 farmers can work one at a time.");
            ui.small("Huts shelter farmers from raids; knights rush to defend an attacked one.");
            ui.small("Rally: knights drop combat and rush the flag; it clears on arrival.");
            ui.small("Right-click clears the rally flag.");
            ui.small("Pan with WASD or the arrow keys; scroll to zoom.");
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
