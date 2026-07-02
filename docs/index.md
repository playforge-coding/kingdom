---
comments: true
---

# Kingdom

**Kingdom** is a small single-player kingdom-builder written in Rust. You don't
control a character — you watch your settlement from a top-down view, grow it,
and defend it against rival camps that share your continent.

It renders tiles and entities with **wgpu**, draws its HUD with **egui**, and
runs both natively and in the browser (via WebGL) using **[Trunk](https://trunkrs.dev)**.

<div class="grid cards" markdown>

- :material-rocket-launch: **[Getting Started](getting-started.md)**

- :material-controller: **[Controls & HUD](controls.md)**

- :material-crown: **[Gameplay](gameplay.md)**

- :material-hammer-wrench: **[Building](building.md)**

- :material-sword-cross: **[Units & Combat](units.md)**

- :material-earth: **[The World](world.md)**

- :material-content-save: **[Saves & Files](saves.md)**

</div>

## At a glance

|                 |                                                                  |
| --------------- | ---------------------------------------------------------------- |
| **Genre**       | Single-player, top-down kingdom-builder / RTS-lite               |
| **Renderer**    | wgpu (native WebGPU / Vulkan / Metal / DX; WebGL in the browser) |
| **UI**          | egui overlay                                                     |
| **World**       | Infinite seeded continents in 32×32-tile chunks, ringed by ocean |
| **You control** | The whole settlement — units act on their own AI                 |
| **Opponents**   | Enemy villages that keep founding more, ever farther out         |
| **Allies**      | Overseas trade partners that fight the enemy, not you            |
| **Hazards**     | Rare pirate ships that shell your cargo shipments at sea         |
| **Persistence** | Custom binary `.dat` blob (native file / web IndexedDB)          |
| **Platforms**   | Native (Windows/macOS/Linux) and web (WebAssembly)               |

!!! tip "New here?"
Head to **[Getting Started](getting-started.md)** to get the game running,
then skim **[Controls & HUD](controls.md)** and **[Gameplay](gameplay.md)**
before you found your first extra house.
