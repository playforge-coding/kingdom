# Kingdom

A small single-player kingdom-builder written in Rust. You don't control a
character — you watch your settlement from a top-down view, grow it, and defend
it against rival camps. It renders tiles and entities with **wgpu** and runs
both natively and in the browser (via WebGL) using **Trunk**.

![tiles rendered with wgpu, egui overlay](assets/textures/tiles/houses.png)

> 📖 **Documentation:** <https://playforge-coding.github.io/kingdom/>
> · 🎮 **Play in your browser:** <https://playforge-coding.github.io/kingdom/play/>

## Gameplay

- **Main menu & world creation.** Start from a menu: enter a **seed** (or roll a
  random one) and create a world, **load** your saved world, or resume. A set
  seed is fully reproducible.
- **Infinite, chunked world.** The map is divided into 32×32 tile chunks
  generated on demand from the seed as you explore, so the world is effectively
  endless. Terrain uses [FastNoise Lite](https://crates.io/crates/fastnoise-lite):
  an elevation field decides land vs. water and a second noise field scatters
  forests (wood) and ore (stone). Narrow water channels are automatically
  spanned with **bridges** so landmasses stay connected.
- **Saving.** Save any time to a custom binary `.dat` blob (magic `KGDM`) that
  captures the seed, stockpile, stats, every unit, and all edited chunks
  (buildings, bridges, depleted resources). Native builds write a file
  (`kingdom_save.dat`); the web build stores the blob in **IndexedDB**.
- **Farmers** walk to the nearest reachable resource, then **chop wood** and
  **mine stone** (with a swing animation). Everything they gather goes into your
  shared stockpile.
- **Knights** seek out and **attack** the enemy faction. Both sides take damage
  in melee; HP bars appear above wounded units. Set a **rally flag** (left-click
  in rally mode; right-click to clear) to pull them to a point.
- **Three factions** (you = blue, enemy = red, allies = green). The **enemy**
  holds **four villages** and is hostile to everyone; their soldiers stream out
  to hunt you. The **allies** hold villages on far-off coasts: they trade with
  you and attack the enemy on their own, but never join your battles and never
  fight you.
- **Your village.** You begin controlling a single village near the origin.
  Villages change hands per-camp: leave one undefended with an enemy inside and
  you lose it (your knights auto-rally to retake it); strip an enemy village of
  its defenders and stand a unit in it to capture it. (Allied villages can't be
  captured.)
- **Priorities.** Toggle **Agriculture / Military** to bias which workers your
  houses raise, and **Balanced / Wood / Stone** to steer what farmers gather.
- **Economy.** Alongside wood and stone you keep a purse of **gold**. You start
  with some seed gold; every new **knight** costs gold to arm (a broke village
  raises a free farmer instead), and most structures cost gold too.
- **Trade.** Load wood and stone onto a **cargo ship** and left-click open water
  to launch it. It charts a water route to the **nearest allied coast** (never
  crossing land) and sells the goods for gold on arrival — **stone fetches more
  than wood**.
- **Building.** Left-click to build:
  - a **House** on open grass (wood + stone + gold) — only *next to your existing
    village*, so your territory grows organically. Houses raise your population
    cap and spawn new workers (while you have 4+ farmers).
  - a **Bridge** on open water (wood) — makes that tile walkable.
  - a **Mine** on open ground (stone + gold) — a bottomless stone source, worked
    by up to four farmers at once.
  - a **Wall** (wood + stone + gold) — a defensive blocker units path around.
  - a **Hut** (click a tree, gold) — a knight builds it; it shelters farmers from
    raids, and knights rush to defend an attacked one.
- Units navigate with grid **pathfinding** (BFS) and treat water, buildings and
  resource nodes as obstacles, routing around them or across bridges.

## Controls

| Input | Action |
|-------|--------|
| `WASD` / arrow keys | Pan the camera |
| Mouse scroll | Zoom in / out |
| Left-click | Perform the current build action / place the rally flag |
| Right-click | Clear the rally flag |
| `Esc` | Return to menu (from the menu: quit on native) |

The **egui** panel in the top-left shows your stockpile, population, score, and
the current build mode, plus priority toggles and **Save** and **Menu** buttons.

## Running

### Native

```sh
cargo run --release
```

### Web

Requires [Trunk](https://trunkrs.dev) and the wasm target:

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked trunk

trunk serve            # dev server at http://127.0.0.1:8080
# or
trunk build --release  # static bundle in dist/
```

wgpu is built with the `webgl` feature so the game runs on browsers without
native WebGPU support.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Entry point (native + wasm), logging/panic hooks |
| `src/app.rs` | winit event loop, input, camera, per-frame draw list + animation |
| `src/gfx.rs` | wgpu instanced sprite-batch renderer + egui render pass |
| `src/atlas.rs` | Packs all PNGs into one texture atlas; slices animation frames |
| `src/world.rs` | Chunked infinite world, worldgen, bridges, walkability |
| `src/pathfind.rs` | Bounded BFS grid pathfinding |
| `src/game.rs` | Simulation: factions, AI, gathering, combat, spawns, building, save (de)serialization |
| `src/save.rs` | `.dat` persistence (native file / web IndexedDB) |
| `src/camera.rs` | Top-down orthographic camera (tile units) |
| `src/ui.rs` | egui menu + in-game panel |
| `src/shader.wgsl` | Instanced textured-quad shader |
| `assets/textures/` | Tile, building and character-sheet PNGs |
| `docs/` | Documentation site sources (Zensical / Material for MkDocs) |
| `scripts/fetch-fonts.py` | Downloads the docs' self-hosted webfonts |

## Documentation

The docs live in `docs/` and are built with
[Zensical](https://zensical.org) (Material for MkDocs). Fonts are self-hosted
rather than loaded from the Google Fonts CDN, so fetch them once before building:

```sh
pip install zensical
python scripts/fetch-fonts.py   # downloads docs/fonts/ + docs/stylesheets/fonts.css
zensical serve                  # live preview
zensical build --strict         # static site into site/
```

Pushing to the default branch builds the docs **and** the WebAssembly game and
publishes both to GitHub Pages (see [`.github/workflows/docs.yml`](.github/workflows/docs.yml)):

- 📖 Docs — <https://playforge-coding.github.io/kingdom/>
- 🎮 Play — <https://playforge-coding.github.io/kingdom/play/>

Pushing a `v*` tag builds native binaries for Windows, macOS and Linux and
attaches them to a GitHub Release (see [`.github/workflows/release.yml`](.github/workflows/release.yml)).

## Art

The sprites are [MiniWorld Sprites](https://opengameart.org/content/miniworld-sprites)
by [Shade](https://opengameart.org/users/shade-1) on OpenGameArt.org, released
under **CC0** — thanks to Shade, who appreciates the credit.

Character art is stored as 5×12 grids of 16×16 frames; the renderer slices out
walk and action (chop / mine / attack) frames per entity. Trees and rocks are
padded onto a 16×16 tile footprint. Entities are drawn as foreground tiles on
top of the terrain.
