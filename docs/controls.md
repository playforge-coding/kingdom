---
comments: true
---

# Controls & HUD

You never move a character. Instead you steer the *camera* and issue orders
through the panel; your units carry them out on their own.

## Input

| Input | Action |
| ----- | ------ |
| <kbd>W</kbd> <kbd>A</kbd> <kbd>S</kbd> <kbd>D</kbd> / arrow keys | Pan the camera (pan speed scales with zoom) |
| Mouse scroll | Zoom in (up) / out (down) |
| **Left-click** | Perform the current build action, or place the rally flag |
| **Right-click** | Clear the rally flag |
| <kbd>Esc</kbd> | Return to the menu (from the menu on native builds: quit) |

Left-click always does whatever the **Build** section of the panel is currently
set to — place a house, a bridge, a mine, a wall, mark a tree for a hut, or drop
a rally flag. See **[Building](building.md)** for what each mode does.

## The HUD panel

An **egui** window titled *Kingdom* sits in the top-left corner. It's your only
control surface in-game.

### Stockpile

Your shared resources, filled by farmers:

- 🪵 **Wood**
- 🪨 **Stone**

Everything you build is paid for out of this pool.

### Status

- **Population** — current units versus your population cap (raised by houses).
- **Enemies defeated** — running tally of enemy units killed.
- **Units lost** — how many of your own units have died.

### Priority

- **🌾 Agriculture / ⚔ Military** — which kind of villager your houses favour
  raising when they spawn new workers.
- **Gather: Balanced / 🪵 Wood / 🪨 Stone** — biases which resource idle farmers
  seek out. *Balanced* lets them pick the nearest.

### Proclamations

- **📜 Proclaim draft** — declares a draft for a short spell. While it runs,
  your farmers may spontaneously **take up arms as knights** — each still costs
  the usual gold, and an empty treasury stops the call-ups. The button is
  replaced by a countdown while a draft is in force. See
  **[Units & Combat](units.md#the-draft)**.

### Build (left-click)

Radio buttons selecting what a left-click does, with costs shown:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **House** | 8 wood, 8 stone | Must be next to your village |
| **Bridge** | 3 wood | Placed on open water |
| **⛏ Mine** | 12 stone | A bottomless stone source near your village |
| **🧱 Wall** | 2 wood, 4 stone | Defensive blocker |
| **🛖 Hut** | click a tree | A knight walks over and builds it |
| **⚑ Rally knights** | — | Left-click sets a flag your knights rush to |

A **✖ Clear rally** button appears while a rally flag is active.

The HUD's **Trade** and **Navy** sections add two more left-click modes on the
water by your village:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **🚢 Ship goods** | the wood + stone you load | Sails to an allied coast for gold |
| **⚓ Warship** | 20 wood, 10 stone, 60 gold | Patrols, hunts pirates, shells the enemy coast |

### Buttons

- **💾 Save** — write the current world to disk / IndexedDB (see **[Saves & Files](saves.md)**).
- **☰ Menu** — return to the main menu (your session stays resumable).

The panel also lists reminders about the building and unit rules described on
the **[Building](building.md)** and **[Units & Combat](units.md)** pages.
