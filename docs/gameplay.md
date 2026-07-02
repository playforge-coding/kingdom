---
comments: true
---

# Gameplay

Kingdom is a hands-off builder. You don't pilot a hero — you set priorities,
place buildings, and let your farmers and knights act on their own AI. Your job
is to grow a self-sustaining settlement and outlast the enemy camps sharing your
island.

## Worlds and seeds

Everything starts at the main menu:

- **🌍 Create World** — enter a **seed** and generate a fresh island. A set seed
  is fully reproducible: the same seed always yields the same terrain, resources,
  and enemy placements. Leave the field blank (or press **🎲 Random**) to roll a
  random one. Text that isn't a number is hashed into a seed, so `"my castle"`
  is a valid seed too.
- **📂 Load Saved World** — restore your last save (enabled only when one exists).
- **▶ Resume** — jump back into the session you left when you opened the menu.

See **[The World](world.md)** for how a seed becomes terrain.

## Your starting position

You begin controlling **a single village** near the origin — a small cluster of
houses with a handful of **farmers** to gather and **knights** to defend. That
village is the only settlement you own; everything grows outward from it.

Scattered elsewhere on the island are **four enemy villages**, each with its own
units. They aren't idle: enemy soldiers stream out to hunt your units, so you're
on a clock from the start.

## The core loop

1. **Gather.** Farmers walk to the nearest reachable resource and chop wood or
   mine stone. Everything lands in your shared **stockpile**.
2. **Build.** Spend wood and stone on [houses, bridges, mines, walls, and huts](building.md).
   Houses raise your population cap and periodically spawn new workers.
3. **Grow.** More houses → more population → more farmers and knights.
4. **Defend & expand.** Keep knights around to fend off raids, then push into
   enemy territory and take their villages.

Use the **Priority** and **Gather** toggles in the HUD to steer this loop:
lean **Agriculture** while you're building up, swing to **Military** when a camp
starts pressing you.

!!! note "Houses only raise workers while you have 4+ farmers"
    New workers are spawned by houses, but only while your economy can support
    it — you need at least a few farmers gathering before the population grows.

## Capturing villages

Territory changes hands per-village:

- **Taking an enemy village** — if an enemy village has *no defenders of its own
  left* but one of your units is standing inside it, that village converts to
  your control.
- **Losing one of yours** — the same rule applies in reverse. If a village of
  yours is left undefended with an enemy inside it, you lose it. When that
  happens, your knights automatically **rally to the lost village** to try to
  retake it.

This makes defense and offense the same skill: keep a presence in your villages,
and strip the enemy's before you move in.

## Scoring

There's no hard win screen — it's a survival-and-expansion sandbox. The HUD
tracks two numbers that measure how you're doing:

- **Enemies defeated** — total enemy units you've killed (including any trapped
  and finished off).
- **Units lost** — your own casualties.

Grow your kingdom, wipe out the rival camps, and keep your losses low.
