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
| **Left-click** | Perform the current build action, or place a rally flag |
| **Right-click** | Clear every rally flag (the knights' and the navy's) |
| <kbd>Esc</kbd> | Return to the menu (from the menu on native builds: quit) |

Left-click always does whatever the **Build** section of the panel is currently
set to: order a house, a bridge, a mine, or a wall, mark a tree for a hut, mark
a builder zone, or drop a rally flag. Orders aren't built on the spot: a
civilian walks over and builds the site over a few seconds, and clicking a
pending site cancels and refunds it. See **[Building](building.md)** for what
each mode does.

On the **[testing ground](testing.md)** the mouse does something simpler:
left-click places the unit picked in its panel, paints the chosen terrain or
building onto the tile at once, or plants a rally flag, and right-click
removes whoever stands on the tile (or wipes an empty tile back to grass).

## The HUD panel

An **egui** window titled *Kingdom* sits in the top-left corner. It holds your
stockpile and every order you can give. The [Realm panel](#the-realm-panel) on
the right is for finding your way around, and [alerts](#alerts) pop up along
the top.

### Stockpile

Your shared resources, filled by your civilians:

- :lucide-tree-pine: **Wood**
- :lucide-mountain: **Stone**
- :lucide-coins: **Gold**

Everything you build is paid for out of this pool.

### Status

- **Population:** current units versus your population cap (raised by houses).
- **Civilians:** how many are **farming**, **mining**, and **building** right now.
- **Enemies defeated:** running tally of enemy units killed.
- **Units lost:** how many of your own units have died.

### Priority

- **:lucide-wheat: Agriculture / :lucide-swords: Military:** which kind
  of villager your houses favor raising when they spawn new workers. Under
  Military, about a quarter of the soldiers raised are
  **[horsemen](units.md#horsemen)** and a quarter **[bowmen](units.md#bowmen)**
  when the gold allows.
- **Gather: Balanced / :lucide-tree-pine: Wood / :lucide-mountain: Stone:**
  biases which resource idle civilians seek out. *Balanced* lets them pick the nearest.

### Proclamations

- **:lucide-scroll-text: Proclaim draft:** every civilian you can spare
  **takes up arms as a knight on the spot**. They serve as **levies** for a
  4-minute term: free to call up, but each survivor is owed gold once the term
  is up and the fighting is over. While levies are serving, the panel counts
  down their term and shows how many there are and how much you owe them (in
  red if you can't cover it yet). Proclaiming again calls up any newcomers
  and restarts the term. See **[Units & Combat](units.md#the-draft)**.

### Build (left-click)

Radio buttons selecting what a left-click does, with costs shown. Every gold
cost below includes the **builder's 5 gold pay** (bridges are free of it), and
every order waits for a civilian to walk over and build it:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **House** | 8 wood, 8 stone, 35 gold | Within 20 tiles of a house you own |
| **Bridge** | 3 wood | Placed on open water |
| **:lucide-pickaxe: Mine** | 12 stone, 45 gold | A bottomless stone source near your village |
| **:lucide-brick-wall: Wall** | 2 wood, 4 stone, 10 gold | Defensive blocker |
| **:lucide-tent: Hut** | 20 gold | Click a tree; a builder raises it |
| **:lucide-map: Builder zone** | Free | Click to mark (or clear) an area for autonomous building |
| **:lucide-flag: Rally knights** | Free | Left-click plants a flag your knights rush to; plant several and they split up among them |

Below the modes sits the **"Let builders build on their own in zones"** switch:
while it's on, builders keep raising houses inside your
**[builder zones](building.md#builder-zones)**. A **:lucide-x: Clear rally
flags** button appears while any rally flag stands, and while a flag *you* planted
stands, a **:lucide-swords: Hire allied mercenaries** button offers an allied
company that sails over to answer it for gold (see
**[Hiring mercenaries](gameplay.md#hiring-mercenaries)**); the section counts
companies at sea and mercenaries ashore.

The HUD's **Trade**, **Colonies**, and **Navy** sections add five more
left-click modes on the water:

| Mode | Cost | Notes |
| ---- | ---- | ----- |
| **:lucide-ship: Ship goods** | the wood + stone you load | Sails to an allied coast for gold; launches by your village |
| **:lucide-globe: Settler ship** | 30 wood, 20 stone, 50 gold, 3 civilians | Click a far shore; the crew sails over and founds a new village there |
| **:lucide-anchor: Warship** | 20 wood, 10 stone, 60 gold | Patrols, hunts pirates and enemy ships, shells the enemy coast; launches by your village |
| **:lucide-swords: Troop ship** | 25 wood, 5 stone, 20 gold, 4 knights | Click a far shore; the party lands there ready to fight (levies stay home) |
| **:lucide-flag: Rally warships** | Free | Click open water to plant a navy flag; the fleet splits up among your flags, and each lifts when a ship arrives |

The Trade section also holds the **"Send cargo ships out on their own"** switch:
while it's on, the harbor keeps launching laden ships whenever the stockpile
covers the configured load in full, sailing from a different one of your
coastal villages each time (see
**[Autonomous trade](building.md#autonomous-trade)**). A
**:lucide-x: Clear navy flags** button appears in the Navy section while any
navy flag is planted. The Navy section also holds the **:lucide-anchor: Hire
allied escort** button (60 gold), which brings an allied warship over to sail
with your navy (see **[Hiring an escort](gameplay.md#hiring-an-escort)**),
counts the escorts sailing with the fleet, and, while the enemy has ships out,
shows in red how many **enemy troop ships and warships** are at sea (see
**[The enemy navy](units.md#the-enemy-navy)**).

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
  specks, transports and warships in their owner's color (your settler and
  troop ships and warships blue, a hired company or escort green, an enemy
  raid and its escort red), pirates black, the knights' rally flags as yellow
  rings, the navy's rally flags as blue ones, and each voyage's landing as a
  ring in its owner's color, so a red ring is where an enemy raid will come
  ashore. The white box is what's on screen. Land that hasn't been explored
  yet is dark. **Click or drag** on the minimap to move the camera there.
- **:lucide-map: Overview (M):** toggles the [overview](#the-overview).
- **Village count:** how many villages you, the enemy, and the allies hold.
- **Your villages:** one row per village, with where it is relative to the
  camera (for example *120 tiles northeast*, or *In view*), its houses, its
  population against its own cap, and the knights and civilians standing in it.
  A village **under attack** is flagged in
  red and one with **no defenders** in amber, since an empty village is easy to
  take. Those come first in the list. **:lucide-crosshair: Go** jumps the camera
  there and zooms back in if you were on the overview.

## Alerts

When something needs your attention, a toast appears at the top of the screen
saying what happened and where:

- a village is **under attack** (an enemy is in or near it)
- raiders are **attacking a hut** with civilians inside
- a village has **fallen** (your knights rally to retake it)
- you **captured** a village
- knights **settled** a captured village, laying down their arms as civilians
- hostile guns (a pirate's or an enemy warship's) **sank** a cargo ship, a
  settler or troop ship (everyone aboard lost with it), a warship of yours, or
  a hired allied escort
- your settlers or a landing party **set sail** (the ship launches from your
  harbor, which is usually off screen when you click the far shore)
- a settle, troop-landing, or hiring order was **refused**, saying why: no
  shore there, too close to a standing village (settling only), no sea route,
  the cost or crew not covered, or (escorts) the allies have none left to spare
- your settlers **landed** and founded a new village, or your knights went
  **ashore** on a far coast
- an **enemy fleet set sail** for one of your coasts (raised at the landing it's
  bound for), **enemy raiders came ashore** by one of your villages, or an
  **enemy ship went down**
- an allied **escort set sail** to join your navy, or **sailed home** once the
  sea was quiet
- the draft stood down and your levies were **paid**, or some **deserted**
  because you couldn't pay them

Press **:lucide-crosshair: Go** to jump there, or **:lucide-x:** to dismiss it.
Alerts fade after a few seconds. An ongoing raid only raises one alert, and the
same spot won't alert again until it has been quiet for a while.

Some alerts also carry a **one-click action button**, shown only on that kind
of alert and only while you can actually take it:

- The sunk-ship alerts (cargo, settler, and troop alike) carry
  **:lucide-anchor: Summon warship**, shown when the stockpile covers a
  warship's cost (20 wood, 10 stone, 60 gold). It lays one down in your harbor
  and **rallies the whole navy to the wreck**.
- The enemy-fleet alert carries **:lucide-flag: Rally navy**, shown while you
  have a warship or hired escort afloat. It's free and **plants the navy rally
  flag at the enemy's landing**, so the fleet sails to meet the raid.

Taking the action dismisses the toast.

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
