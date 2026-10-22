//! egui integration (input side) plus the main-menu and in-game panels. The GPU
//! side (`egui_wgpu::Renderer`) lives in `gfx`.

use egui::{
    Align2, ClippedPrimitive, Color32, Image, ImageSource, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, WidgetAtom,
};
use glam::Vec2;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::atlas::MapColors;
use crate::game::{
    AlertKind, AlertResponse, BOWMAN_GOLD_COST, BRIDGE_WOOD_COST, BUILD_NEAR_RADIUS, BUILDER_WAGE,
    Brush, BuildMode, ESCORT_HIRE_COST, Faction, Game, GatherPriority, HORSEMAN_GOLD_COST,
    HOUSE_GOLD_COST, HOUSE_STONE_COST, HOUSE_WOOD_COST, HUT_GOLD_COST, Job, KNIGHT_GOLD_COST,
    LEVY_GOLD_PAY, LEVY_SERVICE, LaunchRefusal, MAX_ESCORTS, MINE_GOLD_COST, MINE_STONE_COST,
    Priority, SETTLER_CIVILIANS, SETTLER_CIVILIANS_SPARED, SETTLER_GOLD_COST, SETTLER_STONE_COST,
    SETTLER_WOOD_COST, STONE_PRICE, SandboxTool, TROOP_GOLD_COST, TROOP_KNIGHTS, TROOP_STONE_COST,
    TROOP_WOOD_COST, WALL_GOLD_COST, WALL_STONE_COST, WALL_WOOD_COST, WARSHIP_GOLD_COST,
    WARSHIP_STONE_COST, WARSHIP_WOOD_COST, WOOD_PRICE,
};

/// An action the UI is requesting the app perform this frame.
pub enum Action {
    CreateWorld(i32),
    /// Open the testing ground: an empty gray grid to place units on.
    CreateSandbox,
    Load,
    Resume,
    Save,
    ToMenu,
    /// Move the camera to a spot, zooming back in from the overview if asked.
    Focus {
        pos: Vec2,
        zoom_in: bool,
    },
    ToggleOverview,
}

/// Where the camera is looking, for the minimap and the village list.
#[derive(Clone, Copy)]
pub struct View {
    pub center: Vec2,
    /// Half the view's size in tiles.
    pub half: Vec2,
    /// Zoomed out past full detail.
    pub overview: bool,
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
    pub const CASTLE: ImageSource = include_image!("../assets/icons/castle.svg");
    pub const COINS: ImageSource = include_image!("../assets/icons/coins.svg");
    pub const CROSSHAIR: ImageSource = include_image!("../assets/icons/crosshair.svg");
    pub const DICES: ImageSource = include_image!("../assets/icons/dices.svg");
    pub const FLAG: ImageSource = include_image!("../assets/icons/flag.svg");
    pub const FLASK_CONICAL: ImageSource = include_image!("../assets/icons/flask-conical.svg");
    pub const FOLDER_OPEN: ImageSource = include_image!("../assets/icons/folder-open.svg");
    pub const GLOBE: ImageSource = include_image!("../assets/icons/globe.svg");
    pub const MAP: ImageSource = include_image!("../assets/icons/map.svg");
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
    pub const TRIANGLE_ALERT: ImageSource = include_image!("../assets/icons/triangle-alert.svg");
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
    Game {
        game: &'a mut Game,
        view: View,
        colors: &'a MapColors,
    },
}

pub struct Ui {
    pub ctx: egui::Context,
    state: egui_winit::State,
    minimap: Minimap,
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
        Ui {
            ctx,
            state,
            minimap: Minimap::default(),
        }
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
            if let Some(a) = build_ui(ui, &mut scene, &mut self.minimap) {
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

fn build_ui(ui: &mut egui::Ui, scene: &mut Scene, minimap: &mut Minimap) -> Option<Action> {
    match scene {
        Scene::Menu {
            menu,
            has_save,
            has_game,
        } => menu_ui(ui, menu, *has_save, *has_game),
        Scene::Game { game, view, colors } => {
            let ctx = ui.ctx().clone();
            // The testing ground has no stockpile, orders, or realm to keep
            // track of: its one panel is the unit picker.
            if game.sandbox() {
                let hud = sandbox_ui(&ctx, game, colors);
                let alert = alerts_ui(&ctx, game, *view);
                return hud.or(alert);
            }
            let hud = game_ui(&ctx, game);
            let realm = realm_ui(&ctx, game, *view, colors, minimap);
            let alert = alerts_ui(&ctx, game, *view);
            hud.or(realm).or(alert)
        }
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

            ui.add_space(24.0);
            if ui
                .button((icon(ui, icons::FLASK_CONICAL), "Testing Ground"))
                .on_hover_text(
                    "An empty gray grid with nobody on it. Click to place units of any \
                     faction and type and watch how they behave. It is never saved.",
                )
                .clicked()
            {
                action = Some(Action::CreateSandbox);
            }
        });
    });
    action
}

/// The testing ground's panel: pick a faction and a unit type to place with
/// left-click, a terrain or structure brush to paint instead, or the rally
/// flag, and see who's on the grid.
fn sandbox_ui(ctx: &egui::Context, game: &mut Game, colors: &MapColors) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Testing Ground")
        .default_pos([12.0, 12.0])
        .resizable(false)
        .show(ctx, |ui| {
            ui.add(WidgetAtom::new((
                icon(ui, icons::FLASK_CONICAL),
                RichText::new("Place (left-click)").heading(),
            )));
            ui.small(
                "Left-click a tile to put the chosen unit, terrain or building down; \
                 everything stands at once. Right-click a tile to remove whoever stands \
                 on it, or to wipe it back to grass. Units behave as they would in a \
                 real game.",
            );
            ui.separator();

            ui.label("Faction");
            ui.horizontal(|ui| {
                for (faction, name) in [
                    (Faction::Player, "Yours"),
                    (Faction::Enemy, "Enemy"),
                    (Faction::Ally, "Allied"),
                ] {
                    let swatch = RichText::new(name).color(faction_color32(colors, faction));
                    ui.selectable_value(&mut game.place_faction, faction, swatch);
                }
            });
            ui.small("Units, houses, walls and huts belong to this faction.");
            ui.separator();

            ui.radio_value(
                &mut game.sandbox_tool,
                SandboxTool::PlaceUnit,
                RichText::new("Place a unit").strong(),
            );
            ui.horizontal_wrapped(|ui| {
                for (job, name) in [
                    (Job::Civilian, "Civilian"),
                    (Job::Knight, "Knight"),
                    (Job::Horseman, "Horseman"),
                    (Job::Bowman, "Bowman"),
                ] {
                    if ui
                        .selectable_value(&mut game.place_job, job, name)
                        .clicked()
                    {
                        game.sandbox_tool = SandboxTool::PlaceUnit;
                    }
                }
            });

            ui.radio_value(
                &mut game.sandbox_tool,
                SandboxTool::PlaceTile,
                RichText::new("Paint terrain or a building").strong(),
            );
            ui.horizontal_wrapped(|ui| {
                for (brush, name) in [
                    (Brush::Grass, "Grass"),
                    (Brush::Water, "Water"),
                    (Brush::Tree, "Tree"),
                    (Brush::Rock, "Rock"),
                    (Brush::Bridge, "Bridge"),
                    (Brush::House, "House"),
                    (Brush::Wall, "Wall"),
                    (Brush::Hut, "Hut"),
                    (Brush::Mine, "Mine"),
                ] {
                    if ui
                        .selectable_value(&mut game.place_brush, brush, name)
                        .clicked()
                    {
                        game.sandbox_tool = SandboxTool::PlaceTile;
                    }
                }
            });
            ui.small(
                "Painting replaces whatever the tile held. Grass is the eraser. Houses \
                 raise no villagers here; place the people yourself.",
            );

            ui.radio_value(
                &mut game.sandbox_tool,
                SandboxTool::Rally,
                (
                    icon(ui, icons::FLAG),
                    RichText::new("Rally your knights").strong(),
                ),
            );
            if !game.rallies().is_empty()
                && ui
                    .button((
                        icon(ui, icons::X),
                        format!(
                            "Clear {}",
                            plural(game.rallies().len(), "rally flag", "rally flags")
                        ),
                    ))
                    .clicked()
            {
                game.clear_rally();
            }
            ui.small(
                "Plant as many flags as you like: your soldiers split up among them, nearest \
                 first, and a flag short of its share draws more.",
            );
            ui.separator();

            ui.heading("On the grid");
            let (yours, enemy, allied) = game.faction_counts();
            ui.label(
                RichText::new(format!("Yours: {yours}"))
                    .color(faction_color32(colors, Faction::Player)),
            );
            ui.label(
                RichText::new(format!("Enemy: {enemy}"))
                    .color(faction_color32(colors, Faction::Enemy)),
            );
            ui.label(
                RichText::new(format!("Allied: {allied}"))
                    .color(faction_color32(colors, Faction::Ally)),
            );
            ui.label(format!("Enemies defeated: {}", game.enemies_defeated));
            ui.label(format!("Units lost: {}", game.units_lost));
            ui.horizontal(|ui| {
                if ui.button((icon(ui, icons::X), "Clear everyone")).clicked() {
                    game.clear_units();
                }
                if ui
                    .button((icon(ui, icons::X), "Clear the grid"))
                    .on_hover_text("Wipes every painted tile back to grass; units stay")
                    .clicked()
                {
                    game.clear_terrain();
                }
            });
            ui.separator();

            ui.small("The testing ground is never saved.");
            if ui.button((icon(ui, icons::MENU), "Menu")).clicked() {
                action = Some(Action::ToMenu);
            }
        });
    action
}

fn game_ui(ctx: &egui::Context, game: &mut Game) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Kingdom")
        .default_pos([12.0, 12.0])
        .resizable(false)
        .show(ctx, |ui| {
            // The panel has grown taller than small screens, so it scrolls
            // rather than running off the bottom.
            egui::ScrollArea::vertical()
                .max_height(ctx.content_rect().height() - 72.0)
                .show(ui, |ui| {
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
            let (farming, mining, building) = game.role_counts();
            ui.small(format!(
                "Civilians: {farming} farming, {mining} mining, {building} building"
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
            if ui
                .button((icon(ui, icons::SCROLL_TEXT), "Proclaim draft"))
                .on_hover_text(
                    "Every civilian you can spare takes up arms this instant. Proclaim \
                     again to call up newcomers and start the term afresh.",
                )
                .clicked()
            {
                game.proclaim_draft();
            }
            if let Some(secs) = game.levy_term_remaining() {
                icon_label(
                    ui,
                    icons::SCROLL_TEXT,
                    format!("Levies serve {:.0}s more", secs.ceil()),
                );
            }
            let (levies, owed) = game.levies_owed();
            if levies > 0 {
                let tint = if game.money >= owed { ui.visuals().text_color() } else { ALARM };
                ui.horizontal(|ui| {
                    ui.add(icon(ui, icons::COINS).tint(tint));
                    ui.label(
                        RichText::new(format!("{levies} levies serving, {owed} gold owed"))
                            .color(tint),
                    );
                });
            }
            ui.small(format!(
                "Draft: every civilian you can spare takes up arms at once, for free, and \
                 serves {:.0} minutes. Once the term is up and the realm is quiet, each \
                 survivor is paid {LEVY_GOLD_PAY} gold and goes back to work. Any you \
                 can't pay will desert.",
                LEVY_SERVICE / 60.0
            ));
            ui.separator();

            ui.heading("Build (left-click)");
            ui.small(format!(
                "A click places an order; a civilian walks over and builds it. Gold \
                 costs include the builder's {BUILDER_WAGE} gold pay (bridges are free of \
                 it). Click a pending site to cancel and refund it."
            ));
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::House,
                format!(
                    "House  ({HOUSE_WOOD_COST} wood, {HOUSE_STONE_COST} stone, {} gold)",
                    HOUSE_GOLD_COST + BUILDER_WAGE
                ),
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
                    format!(
                        "Mine  ({MINE_STONE_COST} stone, {} gold)",
                        MINE_GOLD_COST + BUILDER_WAGE
                    ),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Wall,
                (
                    icon(ui, icons::BRICK_WALL),
                    format!(
                        "Wall  ({WALL_WOOD_COST} wood, {WALL_STONE_COST} stone, {} gold)",
                        WALL_GOLD_COST + BUILDER_WAGE
                    ),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Hut,
                (
                    icon(ui, icons::TENT),
                    format!("Hut  (click a tree; {} gold)", HUT_GOLD_COST + BUILDER_WAGE),
                ),
            );
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::AutoZone,
                (
                    icon(ui, icons::MAP),
                    "Builder zone  (click to mark or clear an area)",
                ),
            );
            ui.checkbox(
                &mut game.auto_build,
                "Let builders build on their own in zones",
            );
            ui.small("While this is on, builders keep raising houses inside your zones.");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Rally,
                (icon(ui, icons::FLAG), "Rally knights"),
            );
            if !game.rallies().is_empty()
                && ui
                    .button((
                        icon(ui, icons::X),
                        format!("Clear {}", plural(game.rallies().len(), "rally flag", "rally flags")),
                    ))
                    .clicked()
            {
                game.clear_rally();
            }
            ui.small(
                "Plant as many flags as you like: your soldiers split up among them, nearest \
                 first, and a flag short of its share draws more. Each lifts once someone \
                 arrives.",
            );
            // Hired help: an allied company that answers the flag.
            if game.player_flag()
                && ui
                    .button((
                        icon(ui, icons::SWORDS),
                        format!(
                            "Hire allied mercenaries ({} gold)",
                            crate::game::MERC_HIRE_COST
                        ),
                    ))
                    .clicked()
            {
                game.hire_mercenaries();
            }
            let mercs = game.mercenaries();
            let companies = game
                .transports()
                .iter()
                .filter(|t| t.faction == Faction::Ally)
                .count();
            if mercs > 0 || companies > 0 {
                icon_label(
                    ui,
                    icons::SWORDS,
                    format!(
                        "{} ashore, {} at sea",
                        plural(mercs, "mercenary", "mercenaries"),
                        plural(companies, "allied company", "allied companies")
                    ),
                );
            }
            ui.small(format!(
                "With a flag planted, {} allied knights ship over to answer it for {} gold. \
                 They fight beside you and hold your villages, then sail home once it's \
                 quiet. Only your own units can capture a village.",
                crate::game::MERC_KNIGHTS,
                crate::game::MERC_HIRE_COST
            ));
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
            ui.checkbox(
                &mut game.auto_trade,
                "Send cargo ships out on their own",
            );
            ui.small("While this is on, a ship launches with the load above whenever the stockpile covers it in full.");
            ui.separator();

            ui.heading("Colonies");
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::Settle,
                (
                    icon(ui, icons::GLOBE),
                    format!(
                        "Settler ship  ({SETTLER_WOOD_COST} wood, {SETTLER_STONE_COST} stone, {SETTLER_GOLD_COST} gold)"
                    ),
                ),
            );
            ui.small(format!(
                "Click a far shore to settle it: {SETTLER_CIVILIANS} civilians board a ship in \
                 your harbor, sail over, and found a new village on landing. Click one of \
                 your own villages instead and the settlers reinforce it, joining it rather \
                 than founding anew. At least \
                 {SETTLER_CIVILIANS_SPARED} civilians always stay home, and pirates can sink the \
                 ship, settlers and all."
            ));
            // Say when an expedition is actually under way: the ship casts off
            // from the harbor, usually nowhere near where the click landed.
            let at_sea = game
                .transports()
                .iter()
                .filter(|t| t.civilians > 0)
                .count();
            if at_sea > 0 {
                icon_label(
                    ui,
                    icons::SHIP,
                    format!(
                        "{} at sea, each bound for its banner",
                        plural(at_sea, "expedition", "expeditions")
                    ),
                );
            }
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
            ui.small("Launch on open water by your village; it hunts pirates and enemy ships and shells enemies ashore.");
            // Hired help afloat: an allied warship that sails with the navy.
            if ui
                .button((
                    icon(ui, icons::ANCHOR),
                    format!("Hire allied escort ({ESCORT_HIRE_COST} gold)"),
                ))
                .on_hover_text(format!(
                    "An allied warship sails from the nearest allied port to your harbor and \
                     fights alongside your navy, answering the navy rally flag, until the sea \
                     has been quiet a while. At most {MAX_ESCORTS} at once."
                ))
                .clicked()
            {
                game.hire_escort();
            }
            let escorts = game.escorts();
            if escorts > 0 {
                icon_label(
                    ui,
                    icons::ANCHOR,
                    format!("{} sailing with the navy", plural(escorts, "allied escort", "allied escorts")),
                );
            }
            // The enemy's fleet, when one is out: raids bound for your coasts
            // and the warships escorting them.
            let raids = game.enemy_raids_at_sea();
            let enemy_warships = game.warship_count(Faction::Enemy);
            if raids > 0 || enemy_warships > 0 {
                ui.horizontal(|ui| {
                    ui.add(icon(ui, icons::TRIANGLE_ALERT).tint(ALARM));
                    ui.label(
                        RichText::new(format!(
                            "Enemy fleet at sea: {}, {}",
                            plural(raids, "troop ship", "troop ships"),
                            plural(enemy_warships, "warship", "warships")
                        ))
                        .color(ALARM),
                    );
                });
            }
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::LandTroops,
                (
                    icon(ui, icons::SWORDS),
                    format!(
                        "Troop ship  ({TROOP_WOOD_COST} wood, {TROOP_STONE_COST} stone, {TROOP_GOLD_COST} gold)"
                    ),
                ),
            );
            ui.small(format!(
                "Click a far shore and {TROOP_KNIGHTS} knights board a ship and land there, \
                 ready to fight (levies stay home). Once a captured village is quiet, most of \
                 the knights in it lay down their arms and settle it."
            ));
            let landing = game
                .transports()
                .iter()
                .filter(|t| t.faction == Faction::Player && t.knights > 0)
                .count();
            if landing > 0 {
                icon_label(
                    ui,
                    icons::SWORDS,
                    format!(
                        "{} at sea, each bound for its banner",
                        plural(landing, "landing party", "landing parties")
                    ),
                );
            }
            ui.radio_value(
                &mut game.build_mode,
                BuildMode::NavyRally,
                (icon(ui, icons::FLAG), "Rally warships"),
            );
            if !game.navy_rallies().is_empty()
                && ui
                    .button((
                        icon(ui, icons::X),
                        format!(
                            "Clear {}",
                            plural(game.navy_rallies().len(), "navy flag", "navy flags")
                        ),
                    ))
                    .clicked()
            {
                game.clear_navy_rally();
            }
            ui.small(
                "Click open water to plant navy flags; the warships split up among them, \
                 nearest first, and each flag lifts when a ship arrives.",
            );
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button((icon(ui, icons::SAVE), "Save")).clicked() {
                    action = Some(Action::Save);
                }
                if ui.button((icon(ui, icons::MENU), "Menu")).clicked() {
                    action = Some(Action::ToMenu);
                }
            });

            egui::CollapsingHeader::new("Help")
                .default_open(false)
                .show(ui, |ui| {
                ui.small(format!(
                    "Structures go up to {BUILD_NEAR_RADIUS} tiles from a house you own."
                ));
                ui.small("With 3 or fewer civilians, houses raise workers only and growth is slowest.");
                ui.small(format!(
                    "Each new knight costs {KNIGHT_GOLD_COST} gold; a broke village raises a civilian instead."
                ));
                ui.small(format!(
                    "Under Military priority, about a quarter of your soldiers are horsemen at {HORSEMAN_GOLD_COST} gold each and a quarter bowmen at {BOWMAN_GOLD_COST}."
                ));
                ui.small("Horsemen are fast and tough and leap streams up to 2 tiles wide, but never hack, bridge, or board ships.");
                ui.small("Bowmen shoot foes from 5 tiles away, even across water, but are frail in melee and never hack, bridge, or board ships.");
                ui.small("Trade: load a ship on the coast and it sails to an allied shore for gold.");
                ui.small("Autonomous trade keeps launching ships while the stockpile covers the load.");
                ui.small("Settlers: click a far shore and a ship carries civilians over to found a new village.");
                ui.small("Settlers sent to one of your own villages join it as reinforcements.");
                ui.small("New villagers are raised at whichever of your villages has the most empty homes.");
                ui.small("Troop ships land knights on far shores; after a capture, most knights settle down as civilians.");
                ui.small("Allies fight the enemy on their own, but never join your battles.");
                ui.small("Civilians farm, mine, and build as needed; knights defend.");
                ui.small("Builds take a few seconds; the builder's pay is part of the cost.");
                ui.small("Civilians replant trees and mine caves once resources run dry.");
                ui.small("Mines never run out, but only 4 miners can work one at a time.");
                ui.small("Huts shelter civilians from raids; knights rush to defend an attacked one.");
                ui.small("Knights lay their own bridges (wood, no gold) when water blocks their way.");
                ui.small("Rally: knights drop combat and rush your flags, splitting up among them nearest first; each clears on arrival.");
                ui.small("Navy rally: warships share their own flags out the same way, guns still hot.");
                ui.small("Right-click clears every rally flag, the knights' and the navy's.");
                ui.small("Big enemy villages send sea raids at your weakest coastal village, with warships as escort.");
                ui.small("Hired allied escorts sail with your navy and follow the navy flag, then go home when it's quiet.");
                ui.small("Some alerts offer a one-click action, like summoning a warship on a sinking.");
                ui.small("Pan with WASD or the arrow keys; zoom with Q and E.");
                ui.small("Press M to jump to the overview map and back.");
                ui.small("Villages off screen keep working and fighting at a slower pace.");
                ui.small("Each village supports its own population; build houses where you want growth.");
                ui.small("A lost village or raided hut only rallies the knights nearby.");
                });
                });
        });
    action
}

/// On-screen size of the minimap, in points.
const MINIMAP_SIZE: f32 = 200.0;
/// Resolution of the minimap texture (it's square).
const MINIMAP_PX: usize = 160;
/// Smallest stretch of the world the minimap covers, in tiles.
const MINIMAP_MIN_SPAN: i32 = 192;
/// The minimap's area snaps to this many tiles, so panning only rebuilds the
/// texture now and then instead of every frame.
const MINIMAP_SNAP: i32 = 32;
/// Seconds between refreshes while the area stays put, to pick up new land.
const MINIMAP_REFRESH: f64 = 2.0;

/// The minimap's terrain texture, rebuilt only when its area moves or it goes
/// stale. Villages, ships and the camera are painted over it live.
#[derive(Default)]
pub struct Minimap {
    tex: Option<TextureHandle>,
    /// (left, top, span) in tiles of the area the texture shows.
    area: (i32, i32, i32),
    built_at: f64,
}

/// A linear map color as egui's sRGB `Color32`.
fn color32(c: [f32; 4]) -> Color32 {
    egui::Rgba::from_rgb(c[0], c[1], c[2]).into()
}

fn faction_color32(colors: &MapColors, f: Faction) -> Color32 {
    color32(match f {
        Faction::Player => colors.player,
        Faction::Enemy => colors.enemy,
        Faction::Ally => colors.ally,
    })
}

const ALARM: Color32 = Color32::from_rgb(255, 110, 95);
const CAUTION: Color32 = Color32::from_rgb(240, 190, 90);

/// The square area of the world the minimap should show: all of the player's
/// villages plus the camera's view, with some margin, snapped to a grid.
fn minimap_area(game: &Game, view: View) -> (i32, i32, i32) {
    let mut lo = view.center - view.half;
    let mut hi = view.center + view.half;
    for v in game
        .villages()
        .iter()
        .filter(|v| v.faction == Faction::Player)
    {
        lo = lo.min(Vec2::new(v.min.0 as f32, v.min.1 as f32));
        hi = hi.max(Vec2::new(v.max.0 as f32 + 1.0, v.max.1 as f32 + 1.0));
    }
    let margin = 24.0;
    let size = (hi - lo) + Vec2::splat(margin * 2.0);
    let span = (size.max_element().ceil() as i32).max(MINIMAP_MIN_SPAN);
    let span = (span + MINIMAP_SNAP - 1) / MINIMAP_SNAP * MINIMAP_SNAP;
    let mid = (lo + hi) * 0.5;
    let snap = |v: f32| (v / MINIMAP_SNAP as f32).round() as i32 * MINIMAP_SNAP;
    (snap(mid.x) - span / 2, snap(mid.y) - span / 2, span)
}

/// Sample the world into the minimap texture, one tile per pixel.
fn minimap_image(game: &Game, colors: &MapColors, area: (i32, i32, i32)) -> egui::ColorImage {
    let (x0, y0, span) = area;
    let world = &game.world;
    let step = span as f32 / MINIMAP_PX as f32;
    let palette = [
        color32(colors.unexplored),
        color32(colors.water),
        color32(colors.grass),
        color32(colors.tree),
        color32(colors.rock),
    ];
    let mut pixels = Vec::with_capacity(MINIMAP_PX * MINIMAP_PX);
    for py in 0..MINIMAP_PX {
        let y = (y0 as f32 + (py as f32 + 0.5) * step).floor() as i32;
        for px in 0..MINIMAP_PX {
            let x = (x0 as f32 + (px as f32 + 0.5) * step).floor() as i32;
            let shade = if !world.is_generated(x, y) {
                0
            } else if world.tile(x, y) == crate::world::Tile::Water {
                if world.is_bridge(x, y) { 4 } else { 1 }
            } else {
                match world.node(x, y).map(|n| n.kind) {
                    Some(crate::world::Resource::Wood) => 3,
                    Some(crate::world::Resource::Stone) => 4,
                    None => 2,
                }
            };
            pixels.push(palette[shade]);
        }
    }
    egui::ColorImage::new([MINIMAP_PX, MINIMAP_PX], pixels)
}

/// Draw the minimap. Clicking or dragging on it pans the camera there.
fn minimap_ui(
    ui: &mut egui::Ui,
    game: &Game,
    view: View,
    colors: &MapColors,
    minimap: &mut Minimap,
) -> Option<Action> {
    let now = ui.input(|i| i.time);
    let area = minimap_area(game, view);
    if minimap.tex.is_none() || area != minimap.area || now - minimap.built_at > MINIMAP_REFRESH {
        let image = minimap_image(game, colors, area);
        match &mut minimap.tex {
            Some(tex) => tex.set(image, TextureOptions::NEAREST),
            None => {
                minimap.tex = Some(
                    ui.ctx()
                        .load_texture("minimap", image, TextureOptions::NEAREST),
                )
            }
        }
        minimap.area = area;
        minimap.built_at = now;
    }
    let (x0, y0, span) = minimap.area;
    let origin = Vec2::new(x0 as f32, y0 as f32);
    let span = span as f32;

    let (response, painter) = ui.allocate_painter(
        egui::vec2(MINIMAP_SIZE, MINIMAP_SIZE),
        Sense::click_and_drag(),
    );
    let rect = response.rect;
    let to_screen = |p: Vec2| {
        let rel = (p - origin) / span;
        rect.min + egui::vec2(rel.x, rel.y) * rect.size()
    };

    if let Some(tex) = &minimap.tex {
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        painter.image(tex.id(), rect, uv, Color32::WHITE);
    }

    // Villages, sized by house count. Ones under attack get a pulsing ring.
    let pulse = (now * 4.0).sin() as f32 * 0.5 + 0.5;
    for v in game.villages() {
        let at = to_screen(v.center);
        let r = 2.0 + (v.houses as f32).sqrt();
        painter.circle_filled(at, r, faction_color32(colors, v.faction));
        if v.faction == Faction::Player && v.threatened {
            painter.circle_stroke(at, r + 2.0 + pulse * 3.0, Stroke::new(1.5, ALARM));
        }
    }
    for s in game.ships() {
        painter.circle_filled(to_screen(s.pos), 1.5, Color32::WHITE);
    }
    for s in game.transports() {
        let color = faction_color32(colors, s.faction);
        painter.circle_filled(to_screen(s.pos), 1.5, color);
        // The voyage's landing, so you can see where a colony is headed, or
        // which coast an enemy raid is bound for.
        let landing = Vec2::new(s.anchor.0 as f32 + 0.5, s.anchor.1 as f32 + 0.5);
        painter.circle_stroke(to_screen(landing), 3.0, Stroke::new(1.5, color));
    }
    for w in game.warships() {
        painter.circle_filled(to_screen(w.pos), 1.8, faction_color32(colors, w.faction));
    }
    for p in game.pirates() {
        painter.circle_filled(to_screen(p.pos), 1.8, Color32::BLACK);
    }
    for r in game.rallies() {
        painter.circle_stroke(to_screen(r.pos), 3.0, Stroke::new(1.5, CAUTION));
    }
    for &r in game.navy_rallies() {
        painter.circle_stroke(to_screen(r), 3.0, Stroke::new(1.5, color32(colors.player)));
    }

    let seen = Rect::from_min_max(
        to_screen(view.center - view.half),
        to_screen(view.center + view.half),
    );
    painter.rect_stroke(
        seen,
        0.0,
        Stroke::new(1.0, Color32::WHITE),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, Color32::from_gray(90)),
        StrokeKind::Inside,
    );

    let mut action = None;
    if (response.clicked() || response.dragged())
        && let Some(p) = response.interact_pointer_pos()
    {
        let rel = (p - rect.min) / rect.size();
        action = Some(Action::Focus {
            pos: origin + Vec2::new(rel.x, rel.y) * span,
            zoom_in: false,
        });
    }
    response.on_hover_text("Click or drag to move the camera");
    action
}

/// Where `p` lies relative to the camera, like "120 tiles northeast".
fn whereabouts(view: View, p: Vec2) -> String {
    let d = p - view.center;
    if d.x.abs() <= view.half.x && d.y.abs() <= view.half.y {
        return "In view".to_string();
    }
    // The y axis points down the screen, so north is -y.
    let angle = (-d.y).atan2(d.x).to_degrees().rem_euclid(360.0);
    let directions = [
        "east",
        "northeast",
        "north",
        "northwest",
        "west",
        "southwest",
        "south",
        "southeast",
    ];
    let dir = directions[((angle + 22.5) / 45.0) as usize % 8];
    let tiles = ((d.length() / 10.0).round() as i32 * 10).max(10);
    format!("{tiles} tiles {dir}")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The realm panel: the minimap, the overview toggle, and a list of the
/// player's villages with their defenders and a button to jump to each.
fn realm_ui(
    ctx: &egui::Context,
    game: &Game,
    view: View,
    colors: &MapColors,
    minimap: &mut Minimap,
) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Realm")
        .anchor(Align2::RIGHT_TOP, [-12.0, 12.0])
        .resizable(false)
        .show(ctx, |ui| {
            action = minimap_ui(ui, game, view, colors, minimap);
            if ui
                .add(egui::Button::selectable(
                    view.overview,
                    (icon(ui, icons::MAP), "Overview (M)"),
                ))
                .clicked()
            {
                action = Some(Action::ToggleOverview);
            }

            let count = |f: Faction| game.villages().iter().filter(|v| v.faction == f).count();
            ui.small(format!(
                "Villages: {} yours, {} enemy, {} allied",
                count(Faction::Player),
                count(Faction::Enemy),
                count(Faction::Ally)
            ));
            ui.separator();

            ui.add(WidgetAtom::new((
                icon(ui, icons::CASTLE),
                RichText::new("Your villages").heading(),
            )));
            // Trouble first: villages under attack, then undefended ones, then
            // the rest by distance from the camera.
            let mut mine: Vec<_> = game
                .villages()
                .iter()
                .filter(|v| v.faction == Faction::Player)
                .collect();
            mine.sort_by(|a, b| {
                let rank = |v: &crate::game::Village| (!v.threatened, v.knights + v.civilians > 0);
                rank(a).cmp(&rank(b)).then(
                    a.center
                        .distance_squared(view.center)
                        .total_cmp(&b.center.distance_squared(view.center)),
                )
            });
            if mine.is_empty() {
                ui.small("You hold no villages.");
            }
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .show(ui, |ui| {
                    for (i, v) in mine.iter().enumerate() {
                        ui.push_id(i, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button((icon(ui, icons::CROSSHAIR), "Go")).clicked() {
                                    action = Some(Action::Focus {
                                        pos: v.center,
                                        zoom_in: true,
                                    });
                                }
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(whereabouts(view, v.center)).strong());
                                    ui.small(format!(
                                        "{} ({}/{} pop), {}, {}",
                                        plural(v.houses, "house", "houses"),
                                        v.pop,
                                        v.cap,
                                        plural(v.knights, "knight", "knights"),
                                        plural(v.civilians, "civilian", "civilians"),
                                    ));
                                    if v.threatened {
                                        let warn = icon(ui, icons::TRIANGLE_ALERT).tint(ALARM);
                                        ui.add(WidgetAtom::new((
                                            warn,
                                            RichText::new("Under attack").color(ALARM),
                                        )));
                                    } else if v.knights + v.civilians == 0 {
                                        ui.label(
                                            RichText::new("No defenders, easy to take")
                                                .small()
                                                .color(CAUTION),
                                        );
                                    }
                                });
                            });
                        });
                    }
                });
        });
    action
}

/// Recent alerts, stacked at the top of the screen, newest first. Each one can
/// jump the camera to where it happened or be dismissed.
fn alerts_ui(ctx: &egui::Context, game: &mut Game, view: View) -> Option<Action> {
    let mut action = None;
    let mut dismiss = None;
    let mut respond = None;
    egui::Area::new(egui::Id::unique("alerts"))
        .anchor(Align2::CENTER_TOP, [0.0, 12.0])
        .show(ctx, |ui| {
            for (i, alert) in game.alerts().iter().enumerate().rev() {
                // Fade out over the last few seconds on screen.
                let opacity = (alert.freshness() * 4.0).min(1.0);
                ui.push_id(i, |ui| {
                    ui.multiply_opacity(opacity);
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (glyph, tint, text) = match alert.kind {
                                AlertKind::VillageThreatened => {
                                    (icons::TRIANGLE_ALERT, ALARM, "A village is under attack")
                                }
                                AlertKind::HutRaided => {
                                    (icons::TENT, ALARM, "Raiders are attacking a hut")
                                }
                                AlertKind::VillageLost => (
                                    icons::FLAG,
                                    ALARM,
                                    "A village has fallen. Your knights are rallying to retake it",
                                ),
                                AlertKind::VillageCaptured => (
                                    icons::CASTLE,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "You captured a village",
                                ),
                                AlertKind::CargoSunk => {
                                    (icons::SHIP, ALARM, "A cargo ship was sunk, its cargo lost")
                                }
                                AlertKind::SettlersSailed => (
                                    icons::GLOBE,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Settlers set sail for the far shore",
                                ),
                                AlertKind::TroopsSailed => (
                                    icons::SWORDS,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "A landing party set sail for the far shore",
                                ),
                                AlertKind::LaunchRefused(why) => (
                                    icons::GLOBE,
                                    CAUTION,
                                    match why {
                                        LaunchRefusal::Stockpile => {
                                            "Can't launch: the stockpile doesn't cover an expedition"
                                        }
                                        LaunchRefusal::Civilians => {
                                            "Can't settle: too few civilians to spare a crew"
                                        }
                                        LaunchRefusal::Knights => {
                                            "Can't land troops: too few standing knights (levies stay home)"
                                        }
                                        LaunchRefusal::NoShore => {
                                            "Can't launch: no shore to land on there"
                                        }
                                        LaunchRefusal::TooClose => {
                                            "Can't settle: too close to a standing village"
                                        }
                                        LaunchRefusal::NoRoute => {
                                            "Can't launch: no sea route reaches that shore"
                                        }
                                        LaunchRefusal::NoRally => {
                                            "Can't hire: plant a rally flag for the company to answer"
                                        }
                                        LaunchRefusal::Mercenaries => {
                                            "Can't hire: no allied village has knights to spare"
                                        }
                                        LaunchRefusal::Escorts => {
                                            "Can't hire: the allies have no more escorts to spare"
                                        }
                                    },
                                ),
                                AlertKind::SettlersSunk => (
                                    icons::SHIP,
                                    ALARM,
                                    "A settler ship was sunk, all souls lost",
                                ),
                                AlertKind::TroopsSunk => (
                                    icons::SWORDS,
                                    ALARM,
                                    "A troop ship was sunk, the knights lost at sea",
                                ),
                                AlertKind::WarshipSunk => {
                                    (icons::ANCHOR, ALARM, "One of your warships was sunk")
                                }
                                AlertKind::EnemyFleetSailed => (
                                    icons::TRIANGLE_ALERT,
                                    ALARM,
                                    "An enemy fleet set sail for this coast",
                                ),
                                AlertKind::EnemyTroopsLanded => (
                                    icons::SWORDS,
                                    ALARM,
                                    "Enemy raiders came ashore by your village",
                                ),
                                AlertKind::EnemyShipSunk => (
                                    icons::ANCHOR,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "An enemy ship went down",
                                ),
                                AlertKind::EscortSailed => (
                                    icons::ANCHOR,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "An allied escort set sail to join your navy",
                                ),
                                AlertKind::EscortSunk => {
                                    (icons::ANCHOR, ALARM, "The allied escort was sunk")
                                }
                                AlertKind::EscortLeft => (
                                    icons::ANCHOR,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "The sea quiet, the allied escort sailed home",
                                ),
                                AlertKind::ColonyFounded => (
                                    icons::GLOBE,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Your settlers landed and founded a village",
                                ),
                                AlertKind::SettlersJoined => (
                                    icons::GLOBE,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Your settlers went ashore and joined the village",
                                ),
                                AlertKind::TroopsLanded => (
                                    icons::SWORDS,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Your knights are ashore on the far coast",
                                ),
                                AlertKind::KnightsSettled => (
                                    icons::WHEAT,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Knights laid down their arms and settled the village",
                                ),
                                AlertKind::ImmigrantsArrived => (
                                    icons::WHEAT,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Allied immigrants came home with the ship and settled in",
                                ),
                                AlertKind::MercenariesSailed => (
                                    icons::SWORDS,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "An allied company set sail for your rally flag",
                                ),
                                AlertKind::MercenariesLanded => (
                                    icons::SWORDS,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "The allied company is ashore and making for your flag",
                                ),
                                AlertKind::MercenariesLeft => (
                                    icons::SHIP,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "Their work done, the allied company sailed home",
                                ),
                                AlertKind::PeopleIncorporated => (
                                    icons::WHEAT,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "The village's people joined your realm",
                                ),
                                AlertKind::PeopleLost => (
                                    icons::TENT,
                                    ALARM,
                                    "Villagers were carried off into the enemy's realm",
                                ),
                                AlertKind::LeviesPaid => (
                                    icons::COINS,
                                    color32([0.3, 0.55, 1.0, 1.0]),
                                    "The levies were paid and went back to farming",
                                ),
                                AlertKind::LeviesDeserted => {
                                    (icons::COINS, ALARM, "Unpaid levies deserted")
                                }
                            };
                            ui.add(icon(ui, glyph).tint(tint));
                            ui.label(RichText::new(text).color(tint));
                            ui.small(format!("({})", whereabouts(view, alert.pos)));
                            if ui.button((icon(ui, icons::CROSSHAIR), "Go")).clicked() {
                                action = Some(Action::Focus {
                                    pos: alert.pos,
                                    zoom_in: true,
                                });
                            }
                            // The alert's one-click action, only while the
                            // realm can actually afford to take it.
                            if let Some(r) = alert.kind.response()
                                && game.can_respond(r)
                            {
                                let (glyph, label, hover) = match r {
                                    AlertResponse::SummonWarship => (
                                        icons::ANCHOR,
                                        "Summon warship",
                                        format!(
                                            "Lays down a warship in your harbor ({WARSHIP_WOOD_COST} wood, \
                                             {WARSHIP_STONE_COST} stone, {WARSHIP_GOLD_COST} gold) and \
                                             rallies the navy to the spot"
                                        ),
                                    ),
                                    AlertResponse::RallyNavy => (
                                        icons::FLAG,
                                        "Rally navy",
                                        "Plants the navy rally flag at the enemy's landing, so every \
                                         warship and escort sails to meet the fleet"
                                            .to_string(),
                                    ),
                                };
                                if ui
                                    .button((icon(ui, glyph), label))
                                    .on_hover_text(hover)
                                    .clicked()
                                {
                                    respond = Some(i);
                                }
                            }
                            if ui.small_button(icon(ui, icons::X)).clicked() {
                                dismiss = Some(i);
                            }
                        });
                    });
                });
            }
        });
    if let Some(i) = respond {
        game.respond_to_alert(i);
    } else if let Some(i) = dismiss {
        game.dismiss_alert(i);
    }
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
