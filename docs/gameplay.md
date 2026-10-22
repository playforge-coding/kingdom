---
comments: true
---

# Gameplay

Kingdom is a hands-off builder. You don't pilot a hero. You set priorities,
place build orders, and let your civilians and knights act on their own AI. Your
job is to grow a self-sustaining settlement and outlast the enemy camps sharing
your continent.

## Worlds and seeds

Everything starts at the main menu:

- **:lucide-globe: Create World:** enter a **seed** and generate a fresh world. A set seed
  is fully reproducible: the same seed always yields the same terrain, resources,
  and enemy placements. Leave the field blank (or press **:lucide-dices: Random**) to roll a
  random one. Text that isn't a number is hashed into a seed, so `"my castle"`
  is a valid seed too.
- **:lucide-folder-open: Load Saved World:** restore your last save (enabled only when one exists).
- **:lucide-play: Resume:** jump back into the session you left when you opened the menu.

See **[The World](world.md)** for how a seed becomes terrain.

## Your starting position

You begin controlling **a single village** on the **coast** near the origin, a
small cluster of houses with a handful of **civilians** to work and **knights**
to defend. It sits right by the sea, giving your cargo ships a home port. That
village is the only settlement you own; everything grows outward from it.

Scattered across your home continent are several **enemy villages**, each with its
own units. They aren't idle: enemy soldiers stream out to hunt your units, so
you're on a clock from the start. And the map never settles: **fresh enemy and
allied villages keep being founded** farther and farther out as you play.

## Factions

Three factions share the world:

- **You** (blue): the settlement you grow and command.
- **The enemy** (red): hostile to everyone. Their villages raid you *and* the
  allies, and you capture ground by clearing their villages of defenders.
- **The allies** (green): a friendly faction on coasts **across the sea**. They
  are your **trade partners**: cargo ships sail to their shores to sell goods for
  gold.
  Allied knights **attack the enemy** on their own, which quietly takes pressure
  off you, but they **never join your battles**, can't be rallied, and you and
  the allies never fight. Allied villages can't be captured by anyone, so they
  make dependable, permanent trade destinations.

## The core loop

1. **Gather.** Civilians walk to the nearest reachable resource and chop wood or
   mine stone. Everything lands in your shared **stockpile**.
2. **Trade.** Load surplus wood and stone onto a [cargo ship](building.md#ship-goods)
   and launch it from your **harbor** to the nearest **allied coast** to sell for
   **:lucide-coins: gold** (stone sells for more). Flip the
   [autonomous-trade switch](building.md#autonomous-trade) and the harbor keeps
   dispatching laden ships on its own. Mind the open water, though:
   **pirates** prowl the ocean and will sink a cargo ship they catch (the alert
   it raises offers a one-click **Summon warship** when you can afford one).
3. **Build.** Spend wood, stone, and gold on [houses, bridges, mines, walls, and huts](building.md).
   A click places an order; a civilian takes up the **builder's** job, walks
   over, and puts the structure up over a few seconds (the builder's pay is part
   of the cost). Houses raise their own village's population cap and
   periodically spawn new workers, so each settlement grows on its own. Mark
   [builder zones](building.md#builder-zones) and flip the autonomous-building
   switch to let builders raise houses there without further clicks.
4. **Grow.** More houses mean more population, and so more civilians and knights.
   You start with some **seed gold**, but every new **knight** costs gold to arm. A
   broke village raises a (free) civilian instead, so keep the trade routes running.
5. **Defend & expand.** Keep knights around to fend off raids, then push into
   enemy territory and take their villages. When the frontier worth having lies
   across the sea, send a **[settler ship](#settling-overseas)** instead and
   found a village of your own on a far shore.

Use the **Priority** and **Gather** toggles in the HUD to steer this loop:
lean **Agriculture** while you're building up, swing to **Military** when a camp
starts pressing you.

!!! note "Houses only raise workers while you have 4 or more civilians"
    New workers are spawned by houses, but only while your economy can support
    it, so you need at least a few civilians gathering before the population grows.

## Capturing villages

Territory changes hands per-village:

- **Taking an enemy village:** if an enemy village has *no defenders of its own
  left* but one of your units is standing inside it, that village converts to
  your control.
- **Losing one of yours:** the same rule applies in reverse. If a village of
  yours is left undefended with an enemy inside it, you lose it. When that
  happens, the knights **near the lost village** automatically rally to try to
  retake it, while garrisons farther away stay at their own posts.

This makes defense and offense the same skill: keep a presence in your villages,
and strip the enemy's before you move in.

### After the battle

A captured village converts whole: its **houses become yours** on the spot, and
the walls around it change hands with them. The army settles in right behind
the conquest: of the knights standing in the village when it falls, **some lay
down their arms and become civilians** so the place is worked rather than
merely occupied. How many depends on the ground:

- **Contested** (enemies still prowling nearby): **half** the knights settle
  and the rest stay under arms.
- **Quiet** (no enemy in the area, as when you walk into an **empty village**):
  **everyone but a single garrison** settles, so the village starts producing
  at once.

Levies never settle (they're owed their pay back home), a lone conqueror always
stays a knight, and an alert tells you when knights have settled a village.

Once you hold several villages, the **[Realm panel](controls.md#the-realm-panel)**
lists each one with its defenders and flags any that are under attack or left
empty, and **[alerts](controls.md#alerts)** tell you when one is raided, lost, or
taken. Press <kbd>M</kbd> for the **[overview](controls.md#the-overview)** to see
the whole realm at once.

## Settling overseas

Walking and bridging only carry you so far: the ocean is too wide to span, so
land across the sea is out of a knight's reach. **Settler ships** are how the
kingdom crosses it.

Pick **Settler ship** in the HUD's **Colonies** section and **left-click a far
shore**. If the stockpile covers the cost (30 wood, 20 stone, 50 gold), the
spot is clear of every standing settlement, and a sea route reaches it from
your harbor, then **3 civilians board a ship** and set sail. At least 4
civilians always stay home, so an expedition can't hollow out your economy.

You'll know the order went through: a **blue banner is planted on the landing**
the moment the ship casts off and stays there until it arrives, an
**[alert](controls.md#alerts)** announces the departure (the ship launches from
your harbor, usually far from where you clicked), and the Colonies section
counts the **expeditions at sea**. A refused click answers too: a toast at the
spot says **why it couldn't go through**, whether the shore is too close to a
settlement, no sea route reaches it, there's no shore there at all, or the cost
or crew isn't covered.

When the ship drops anchor, the settlers go ashore, **raise the colony's first
two houses**, and get to work. The new village is yours in every way: it shows
up in the **[Realm panel](controls.md#the-realm-panel)**, its houses raise
population on their own, and its coast works as a harbor, so **cargo ships can
launch from the colony** too; a colony settled near allied shores makes for
short, safe trade runs.

A few things to mind:

- **Pirates sink settler ships** as readily as cargo ships, and the crew goes
  down with the hull. Escort a precious expedition with a
  [warship](building.md#warship-the-navy) or clear the lanes first.
- A fresh colony starts **undefended**. Knights spawn there like anywhere else
  once there's gold and population headroom, but until then an enemy wandering
  in can [take it](#capturing-villages).
- Settling is for **new ground**: the landing must be well away from every
  existing village, yours, the enemy's, and the allies' alike.

See **[Building](building.md#settler-ship)** for the full launch rules.

## Invading overseas

Colonies claim empty shores; **troop ships** take held ones. Pick **Troop
ship** in the HUD's **Navy** section and **left-click a far shore**, right by
an enemy village if you like: **4 of your standing knights board at the
harbor** (levies stay home) and go ashore at the banner, ready to fight. On
landing they **march on the nearest enemy village** within range on their own
(a self-lifting rally, which never overrides a flag you planted yourself), then
their ordinary instincts take over: they hunt the defenders, batter the huts
and walls, and the moment the village stands undefended with your knights
inside, it's [captured](#capturing-villages) and some of them
[settle it](#after-the-battle). Land at an **empty** village and all of that
happens the moment they walk in. Only a shore with no enemy village within
marching reach leaves the party waiting at the beach for your rally flag.

The same feedback applies as for settling: a banner on the landing, a departure
alert, a landing-parties count in the Navy section, and a toast explaining any
refused click. The same risk applies too: a troop ship is **unarmored**, and
pirates send it down knights and all, so shell the coast clear with
[warships](building.md#warship-the-navy) and escort the crossing. There's no
return trip; the landing force wins its new home or holds the beach.

See **[Building](building.md#troop-ship)** for the full launch rules.

## Scoring

There's no hard win screen. It's a survival-and-expansion sandbox. The HUD
tracks two numbers that measure how you're doing:

- **Enemies defeated:** total enemy units you've killed (including any trapped
  and finished off).
- **Units lost:** your own casualties.

Grow your kingdom, wipe out the rival camps, and keep your losses low.
