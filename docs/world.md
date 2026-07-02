---
comments: true
---

# The World

Every island is generated from a single **seed**, so the same seed always
produces the same map. The world is effectively **endless** — it's built lazily
as you explore.

## Chunks

The map is divided into **32×32-tile chunks**, generated on demand from the seed
as the camera reveals new ground. There's no fixed edge; pan far enough in any
direction and fresh terrain appears. Chunks you've changed (buildings, bridges,
depleted resources) are remembered and saved.

## Terrain

Terrain comes from [FastNoise Lite](https://crates.io/crates/fastnoise-lite):

- An **elevation** noise field decides **land versus water**.
- A second noise field scatters **resources** across the land:
    - **Forests** — trees, your source of 🪵 **wood**.
    - **Ore** — rocks, your source of 🪨 **stone**.

## Bridges

Narrow water channels are **automatically spanned with bridges** at generation
time, so landmasses stay connected and your units can get around without you
micromanaging every crossing. You can build your own [bridges](building.md#bridge)
to cross wider water.

## Resources

- **Trees** and **rocks** occupy a full tile and act as obstacles — units path
  around them.
- Farmers **deplete** nodes as they gather, then move on. They **replant trees**
  and fall back to **mines** when the easy pickings are gone.
- A player-built **[mine](building.md#mine)** is a bottomless stone source (worked
  by up to four farmers at once) for when natural ore runs low.

## Settlements

At world creation the generator places:

- **Your village** near the origin — the settlement you start controlling.
- **Four enemy villages** scattered around the island, each with its own units.

How these change hands is covered in
[Capturing villages](gameplay.md#capturing-villages).
