---
comments: true
---

# The World

Every world is generated from a single **seed**, so the same seed always
produces the same map. The world is effectively **endless** — it's built lazily
as you explore, a sprawl of **continents** scattered across open **ocean**.

## Chunks

The map is divided into **32×32-tile chunks**, generated on demand from the seed
as the camera reveals new ground. There's no fixed edge; pan far enough in any
direction and fresh terrain appears. Chunks you've changed (buildings, bridges,
depleted resources) are remembered and saved.

## Terrain

Terrain comes from [FastNoise Lite](https://crates.io/crates/fastnoise-lite):

- A low-frequency **continental** noise field shapes the big picture — broad
  **continents** separated by wide **oceans** — while a finer field roughens the
  coastlines into ragged, natural shores. A **home continent** is always raised
  around the origin, so you never start adrift at sea.
- A separate noise field scatters **resources** across the land:
    - **Forests** — trees, your source of 🪵 **wood**.
    - **Ore** — rocks, your source of 🪨 **stone**.

## Rivers and lakes

**Rivers lace the land.** A pair of low-frequency noise fields trace winding,
branching **waterways** across every continent — a broad main network fed by
finer tributaries. They meander like real rivers (the coordinates are
domain-warped so the channels bend rather than run straight) and flow **down to
the coast**, opening the interior to the sea. Most rivers are **2–3 tiles wide**:
wide enough to block units on foot, so they're crossed by a **boat** or a
player-built **[bridge](building.md#bridge)**.

Occasional small **lakes** also dot the interior. When your starting village
sits beside a landlocked pool, a channel is carved from it out to the sea so a
cargo ship launched in your harbour can always reach the coast.

Because rivers reach deep inland, your **[navy](building.md#warship-the-navy)**
can patrol them far from the open ocean — sailing up a river to hunt raiders or
shell an enemy town on its banks.

## Bridges

Only **single-tile** water notches are **automatically spanned with bridges** at
generation time. Anything wider — including the 2–3-tile rivers — stays open
water, a genuine barrier you cross with a **[bridge](building.md#bridge)** you
build yourself or by **[ship](building.md#warship-the-navy)**.

## Resources

- **Trees** and **rocks** occupy a full tile and act as obstacles — units path
  around them.
- Farmers **deplete** nodes as they gather, then move on. They **replant trees**
  and fall back to **mines** when the easy pickings are gone.
- A player-built **[mine](building.md#mine)** is a bottomless stone source (worked
  by up to four farmers at once) for when natural ore runs low.

## Settlements

At world creation the generator places:

- **Your village** on the **coast** nearest the origin — the settlement you start
  controlling, right by the water so your cargo ships have a home port.
- A handful of **enemy villages** on your home continent, each with its own units.
- A couple of **allied villages** across the sea — friendly [trade
  partners](gameplay.md#factions) your cargo ships sail to.

Settlements **favour the waterside**: the generator plants villages beside a
river or coast wherever it can, so most towns sit on a waterway — and within
reach of a **[warship](building.md#warship-the-navy)**.

Because the map is endless, it keeps filling in: **new enemy and allied villages
are founded over time**, each planted farther out on an ever-widening frontier
and spaced apart, so the world never runs out of rivals to fight or partners to
trade with. Allied villages are always raised on coasts **across open water** from
you — the only way to reach them is by [ship](building.md#ship-goods).

How these change hands is covered in
[Capturing villages](gameplay.md#capturing-villages).
