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
| <kbd>E</kbd> or <kbd>=</kbd> (hold) | Zoom in |
| <kbd>Q</kbd> or <kbd>-</kbd> (hold) | Zoom out (past full detail, the map turns into the [overview](#the-overview)) |
| <kbd>M</kbd> | Jump to the [overview](#the-overview) and back |
| **Left-click** | Perform the current build action, or place the rally flag |
| **Right-click** | Clear the rally flag |
| <kbd>Esc</kbd> | Return to the menu (from the menu on native builds: quit) |

Left-click always does whatever the **Build** section of the panel is currently
set to: place a house, a bridge, a mine, a wall, mark a tree for a hut, or drop
a rally flag. See **[Building](building.md)** for what each mode does.

## The HUD panel

An **egui** window titled *Kingdom* sits in the top-left corner. It holds your
stockpile and every order you can give. The [Realm panel](#the-realm-panel) on
the right is for finding your way around, and [alerts](#alerts) pop up along
the top.

### Stockpile

Your shared resources, filled by farmers:

- :lucide-tree-pine: **Wood**
- :lucide-mountain: **Stone**
- :lucide-coins: **Gold**

Everything you build is paid for out of this pool.

### Status

- **Population:** current units versus your population cap (raised by houses).
- **Enemies defeated:** running tally of enemy units killed.
- **Units lost:** how many of your own units have died.

### Priority

- **:lucide-wheat: Agriculture / :lucide-swords: Military:** which kind
  of villager your houses favor raising when they spawn new workers.
- **Gather: Balanced / :lucide-tree-pine: Wood / :lucide-mountain: Stone:**
  biases which resource idle farmers seek out. *Balanced* lets them pick the nearest.

### Proclamations

- **:lucide-scroll-text: Proclaim draft:** declares a draft for a short spell.
  While it runs, your farmers may spontaneously **take up arms as knights**. They
  serve as **levies**: free to call up, but each survivor is owed gold once the
  fighting is over. The button is replaced by a countdown while a draft is in
  force, and while levies are serving the panel shows how many there are and
  how much you owe them (in red if you can't cover it yet). See
  **[Units & Combat](units.md#the-draft)**.

### Build (left-click)

Radio buttons selecting what a left-click does, with costs shown:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **House** | 8 wood, 8 stone, 30 gold | Must be next to your village |
| **Bridge** | 3 wood | Placed on open water |
| **:lucide-pickaxe: Mine** | 12 stone, 40 gold | A bottomless stone source near your village |
| **:lucide-brick-wall: Wall** | 2 wood, 4 stone, 5 gold | Defensive blocker |
| **:lucide-tent: Hut** | 15 gold | Click a tree; a knight walks over and builds it |
| **:lucide-flag: Rally knights** | Free | Left-click sets a flag your knights rush to |

A **:lucide-x: Clear rally** button appears while a rally flag is active.

The HUD's **Trade** and **Navy** sections add two more left-click modes on the
water by your village:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **:lucide-ship: Ship goods** | the wood + stone you load | Sails to an allied coast for gold |
| **:lucide-anchor: Warship** | 20 wood, 10 stone, 60 gold | Patrols, hunts pirates, shells the enemy coast |

### Buttons

- **:lucide-save: Save:** write the current world to disk / IndexedDB (see **[Saves & Files](saves.md)**).
- **:lucide-menu: Menu:** return to the main menu (your session stays resumable).

### Help

A collapsed **Help** section at the bottom holds short reminders of the building
and unit rules described on the **[Building](building.md)** and
**[Units & Combat](units.md)** pages. Click it to expand.

## The Realm panel

The *Realm* window in the top-right corner is how you keep track of a large
kingdom:

- **Minimap:** your villages and everything around them. Villages show as dots
  in their faction's color (bigger for more houses), cargo ships as white
  specks, warships blue, pirates black, and the rally flag as a yellow ring. The
  white box is what's on screen. Land that hasn't been explored yet is dark.
  **Click or drag** on the minimap to move the camera there.
- **:lucide-map: Overview (M):** toggles the [overview](#the-overview).
- **Village count:** how many villages you, the enemy, and the allies hold.
- **Your villages:** one row per village, with where it is relative to the
  camera (for example *120 tiles northeast*, or *In view*), its houses, its
  population against its own cap, and the knights and farmers standing in it.
  A village **under attack** is flagged in
  red and one with **no defenders** in amber, since an empty village is easy to
  take. Those come first in the list. **:lucide-crosshair: Go** jumps the camera
  there and zooms back in if you were on the overview.

## Alerts

When something needs your attention, a toast appears at the top of the screen
saying what happened and where:

- a village is **under attack** (an enemy is in or near it)
- raiders are **attacking a hut** with farmers inside
- a village has **fallen** (your knights rally to retake it)
- you **captured** a village
- pirates **sank** a cargo ship or a warship
- the draft stood down and your levies were **paid**, or some **deserted**
  because you couldn't pay them

Press **:lucide-crosshair: Go** to jump there, or **:lucide-x:** to dismiss it.
Alerts fade after a few seconds. An ongoing raid only raises one alert, and the
same spot won't alert again until it has been quiet for a while.

## The overview

Zoom out past the normal limit (or press <kbd>M</kbd>) and the map switches to
a flat **overview**. Terrain is drawn as solid color, every village gets a
tinted box in its faction's color, houses and units become small marks, and
ships keep their sprites, drawn bigger. A village of yours under attack
**flashes its border red**, so trouble is easy to spot from across the map.

The whole realm stays alive wherever the camera is: what's on screen (plus a
margin) runs at full speed, and everything beyond it keeps gathering, fighting,
and growing at a slower tick. Distant villages defend themselves, fall, and can
be captured without you watching, so keep an eye on the
[alerts](#alerts) and the [Realm panel](#the-realm-panel). Unexplored land
fills in over a few moments as you look at it. Clicks still work here, so the
overview is handy for setting a rally flag far away.
