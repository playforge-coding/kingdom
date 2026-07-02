---
comments: true
---

# Kingdom

**Kingdom** is a small single-player kingdom-builder written in Rust. You don't
control a character — you watch your settlement from a top-down view, grow it,
and defend it against rival camps that share your island.

It renders tiles and entities with **wgpu**, draws its HUD with **egui**, and
runs both natively and in the browser (via WebGL) using **[Trunk](https://trunkrs.dev)**.

<div class="grid cards" markdown>

- :material-rocket-launch: **[Getting Started](getting-started.md)**

  Build and run the game natively or in the browser.

- :material-controller: **[Controls & HUD](controls.md)**

  Every key, mouse button, and on-screen panel.

- :material-crown: **[Gameplay](gameplay.md)**

  Seeds, worlds, priorities, capturing villages, and the loop that ties it all together.

- :material-hammer-wrench: **[Building](building.md)**

  Houses, bridges, mines, walls, huts, and where you're allowed to place them.

- :material-sword-cross: **[Units & Combat](units.md)**

  Farmers, knights, enemies, rallying, and how fights are resolved.

- :material-earth: **[The World](world.md)**

  Seeded worldgen, chunks, terrain, resources, and bridges.

- :material-content-save: **[Saves & Files](saves.md)**

  The `.dat` save format, native files, and web IndexedDB storage.

</div>

## At a glance

|                    |                                                                  |
| ------------------ | ---------------------------------------------------------------- |
| **Genre**          | Single-player, top-down kingdom-builder / RTS-lite               |
| **Renderer**       | wgpu (native WebGPU / Vulkan / Metal / DX; WebGL in the browser) |
| **UI**             | egui overlay                                                     |
| **World**          | Infinite, seeded, generated in 32×32-tile chunks                 |
| **You control**    | The whole settlement — units act on their own AI                 |
| **Opponents**      | Four enemy villages scattered around the island                  |
| **Persistence**    | Custom binary `.dat` blob (native file / web IndexedDB)          |
| **Platforms**      | Native (Windows/macOS/Linux) and web (WebAssembly)               |

!!! tip "New here?"
    Head to **[Getting Started](getting-started.md)** to get the game running,
    then skim **[Controls & HUD](controls.md)** and **[Gameplay](gameplay.md)**
    before you found your first extra house.
