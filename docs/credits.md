---
comments: true
---

# Credits

Kingdom is built on a stack of excellent open-source Rust crates. Thank you to
their authors and maintainers.

| Crate | Role in Kingdom |
| ----- | --------------- |
| [wgpu](https://crates.io/crates/wgpu) | GPU rendering (native WebGPU/Vulkan/Metal/DX and browser WebGL) |
| [winit](https://crates.io/crates/winit) | Windowing and input event loop |
| [egui](https://crates.io/crates/egui) + [egui-wgpu](https://crates.io/crates/egui-wgpu) | The in-game HUD and menu overlay |
| [egui_extras](https://crates.io/crates/egui_extras) | Drawing the SVG icons in the HUD |
| [glam](https://crates.io/crates/glam) | Vector / matrix math |
| [image](https://crates.io/crates/image) | Decoding the texture PNGs |
| [fastnoise-lite](https://crates.io/crates/fastnoise-lite) | Procedural terrain and resource noise |
| [bytemuck](https://crates.io/crates/bytemuck) | Zero-copy vertex/instance buffers |
| [Trunk](https://trunkrs.dev) | Bundling the WebAssembly web build |

## Licensing

- **Game code** is licensed under **AGPL-3.0-only** (see the `LICENSE` file in the
  repository).
- **This documentation** is licensed **CC BY-NC-SA 4.0**.

## Art

The sprites are [MiniWorld Sprites](https://opengameart.org/content/miniworld-sprites)
by [Shade](https://opengameart.org/users/shade-1) on OpenGameArt.org. They are
released under **CC0** (public domain), but Shade has said they'd appreciate
credit, so thank you, Shade!

Character art is stored as grids of 16x16 frames; the renderer slices out walk and
action (chop / mine / attack) frames per entity, and trees and rocks are padded
onto a 16x16 tile footprint.

The HUD icons are from [Lucide](https://lucide.dev), released under the ISC
license (see `assets/icons/LICENSE`). They're recolored to white so the game can
tint them to match the text around them.
