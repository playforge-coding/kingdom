# Kingdom

A small single-player kingdom-builder written in Rust. You don't control a
character — you watch your settlement from a top-down view, grow it, and defend
it against a rival camp. It renders tiles and entities with **wgpu** and runs
both natively and in the browser (via WebGL) using **Trunk**.

![tiles rendered with wgpu, egui overlay](assets/textures/tiles/houses.png)

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
  in melee; HP bars appear above wounded units.
- **Enemies** stream out of a red encampment on the far side of the island —
  enemy soldiers hunt your units, so keep some knights around to defend.
- **Your village.** You begin controlling a single village near the origin — the
  only settlement you own. Expand outward from it.
- **Building.** Left-click to build:
  - a **House** on open grass (costs wood + stone) — but only *next to your
    existing village/houses*, so your territory grows organically. Houses raise
    your population cap and periodically spawn new workers.
  - a **Bridge** on open water (costs wood) — makes that tile walkable.
- Units navigate with grid **pathfinding** (BFS) and treat water, buildings and
  resource nodes as obstacles, routing around them or across bridges.

## Controls

| Input | Action |
|-------|--------|
| `WASD` / arrow keys | Pan the camera |
| Mouse scroll | Zoom in / out |
| Left-click | Build (house or bridge, per the panel) |
| `Esc` | Return to menu (from the menu: quit on native) |

The **egui** panel in the top-left shows your stockpile, population, score, and
the current build mode, plus **Save** and **Menu** buttons.

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

## Notes on the art

Character art is stored as 5×12 grids of 16×16 frames; the renderer slices out
walk and action (chop / mine / attack) frames per entity. Trees and rocks are
padded onto a 16×16 tile footprint. Entities are drawn as foreground tiles on
top of the terrain.
