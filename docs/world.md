---
comments: true
---

# The World

Every world is generated from a single **seed**, so the same seed always
produces the same map. The world is effectively **endless**: it's built lazily
as you explore, a sprawl of **continents** scattered across open **ocean**.

## Chunks

The map is divided into **32x32-tile chunks**, generated on demand from the seed
as the camera reveals new ground. There's no fixed edge; pan far enough in any
direction and fresh terrain appears. Chunks you've changed (buildings, bridges,
depleted resources) are remembered and saved.

Untouched chunks don't pile up, either: terrain that nothing has edited and
nobody is near is **dropped from memory** and regenerated from the seed the next
time it's needed, identical down to the last tile. So however far you roam, the
game only holds on to your edits plus the land around the action.

## Terrain

Terrain comes from [FastNoise Lite](https://crates.io/crates/fastnoise-lite):

- A low-frequency **continental** noise field shapes the big picture of broad
  **continents** separated by wide **oceans**, while a finer field roughens the
  coastlines into ragged, natural shores. A **home continent** is always raised
  around the origin, so you never start adrift at sea.
- A separate noise field scatters **resources** across the land:
    - **Forests:** trees, your source of :lucide-tree-pine: **wood**.
    - **Ore:** rocks, your source of :lucide-mountain: **stone**.

## Rivers and lakes

**Rivers lace the land.** A pair of low-frequency noise fields trace winding,
branching **waterways** across every continent: a broad main network fed by
finer tributaries. They meander like real rivers (the coordinates are
domain-warped so the channels bend rather than run straight) and flow **down to
the coast**, opening the interior to the sea. Most rivers are **2-3 tiles wide**:
wide enough to block units on foot, so they're crossed by a **boat** or a
player-built **[bridge](building.md#bridge)**. The narrower tributaries, up to
2 tiles across, are no barrier to **[horsemen](units.md#horsemen)**, who leap
them; the 3-tile main rivers and the lakes stop cavalry too.

Occasional small **lakes** also dot the interior. When your starting village
sits beside a landlocked pool, a channel is carved from it out to the sea so a
cargo ship launched in your harbor can always reach the coast.

Because rivers reach deep inland, your **[navy](building.md#warship-the-navy)**
can patrol them far from the open ocean, sailing up a river to hunt raiders or
shell an enemy town on its banks.

## Bridges

Only **single-tile** water notches are **automatically spanned with bridges** at
generation time. Anything wider, including the 2-3-tile rivers, stays open
water, a genuine barrier you cross with a **[bridge](building.md#bridge)** you
build yourself or by **[ship](building.md#warship-the-navy)**.

## Resources

- **Trees** and **rocks** occupy a full tile and act as obstacles, so units path
  around them.
- Civilians **deplete** nodes as they gather, then move on. They **replant
  trees** and fall back to **mines** when the easy pickings are gone.
- A player-built **[mine](building.md#mine)** is a bottomless stone source (worked
  by up to four miners at once) for when natural ore runs low.

## Settlements

At world creation the generator places:

- **Your village** on the **coast** nearest the origin. This is the settlement
  you start controlling, right by the water so your cargo ships have a home port.
- A handful of **enemy villages** on your home continent, each with its own units.
- A couple of **allied villages** across the sea, friendly [trade
  partners](gameplay.md#factions) your cargo ships sail to.

Settlements **favor the waterside**: the generator plants villages beside a
river or coast wherever it can, so most towns sit on a waterway, within
reach of a **[warship](building.md#warship-the-navy)**.

Because the map is endless, it keeps filling in: **new enemy and allied villages
are founded over time**, each planted farther out on an ever-widening frontier
and spaced apart, so the world never runs out of rivals to fight or partners to
trade with. Allied villages are always raised on coasts **across open water** from
you, so the only way to reach them is by [ship](building.md#ship-goods).

Enemy villages don't stay the size they were founded at, either: a full village
with no fighting at its gates **raises new houses of its own**, up to a limit,
and each house lifts the people it supports. See
[Your starting position](gameplay.md#your-starting-position) for what that
means for the pressure on you.

Settlements also **spread from where they stand**. A full enemy town that has
grown to a real size, or a full allied village, sends out a **founding party**
(a few villagers and a knight) that walks about one village-spacing away and
plants a daughter village there, provided the ground is clear of every other
settlement and a road to it exists. Enemy parties lean toward your nearest
village; allied ones keep to coasts across the water from you, where their
ports belong. The far-frontier founding above goes on, but more slowly, so most
new villages now grow out of old ones.

How these change hands is covered in
[Capturing villages](gameplay.md#capturing-villages), and you can plant
villages of your own on far shores with a
[settler ship](gameplay.md#settling-overseas).
