---
comments: true
---

# Kingdom

**Kingdom** is a small single-player kingdom-builder written in Rust. You don't
control a character. Instead you watch your settlement from a top-down view, grow it,
and defend it against rival camps that share your continent.

It renders tiles and entities with **wgpu**, draws its HUD with **egui**, and
runs both natively and in the browser (via WebGL) using **[Trunk](https://trunkrs.dev)**.

<div class="grid cards" markdown>

- :lucide-rocket: **[Getting Started](getting-started.md)**

- :lucide-gamepad-2: **[Controls & HUD](controls.md)**

- :lucide-crown: **[Gameplay](gameplay.md)**

- :lucide-hammer: **[Building](building.md)**

- :lucide-swords: **[Units & Combat](units.md)**

- :lucide-globe: **[The World](world.md)**

- :lucide-save: **[Saves & Files](saves.md)**

</div>

## At a glance

|                 |                                                                  |
| --------------- | ---------------------------------------------------------------- |
| **Genre**       | Single-player, top-down kingdom-builder / RTS-lite               |
| **Renderer**    | wgpu (native WebGPU / Vulkan / Metal / DX; WebGL in the browser) |
| **UI**          | egui overlay                                                     |
| **World**       | Infinite seeded continents in 32x32-tile chunks, ringed by ocean |
| **You control** | The whole settlement, while units act on their own AI            |
| **Opponents**   | Enemy villages that keep founding more, and raid by sea once big |
| **Allies**      | Overseas trade partners that fight the enemy and hire out ships  |
| **Expansion**   | Capture enemy villages, settle far shores, land knights overseas |
| **Hazards**     | Rare pirate ships that shell any hull at sea, and enemy warships |
| **Persistence** | Custom binary `.dat` blob (native file / web IndexedDB)          |
| **Platforms**   | Native (Windows/macOS/Linux) and web (WebAssembly)               |

!!! tip "New here?"
    Head to **[Getting Started](getting-started.md)** to get the game running,
    then skim **[Controls & HUD](controls.md)** and **[Gameplay](gameplay.md)**
    before you found your first extra house.
