---
comments: true
---

# Building

Pick a build mode in the HUD's **Build** section, then **left-click** a tile to
place a **construction order**. Nothing goes up on the spot: the order's full
cost is set aside at the click, a ghost of the structure marks the site, and one
of your **[civilians](units.md#civilians)** walks over, takes up the builder's
job, and works the site for a few real seconds until the structure stands. The
ghost solidifies as the work progresses, and **clicking a pending site again
cancels the order and refunds everything**.

Everything is paid for from your shared stockpile of wood, stone, and
**:lucide-coins: gold**, and most buildings can only go **within 20 tiles of a
house you already own**. That range is roomy enough to plan a whole district
ahead of time, but your territory still grows outward from what you hold
instead of letting you drop structures anywhere on the map.

!!! note "Most things cost gold, the builder's pay included"
    Alongside wood and stone, most structures also cost **gold**, 5 of which is
    the **builder's pay**, set aside with the order and refunded if you cancel
    it. Every new **knight** costs gold too. Gold is earned by shipping goods
    off to a far-away village (see **[Trade](#ship-goods)**). Bridges are the
    one exception: they stay entirely gold-free (their builder works for free),
    so you can always reach the coast to trade even when broke.

## House

| | |
| --- | --- |
| **Cost** | 8 wood, 8 stone, 35 gold (builder's pay included) |
| **Where** | Open grass, within 20 tiles of a house you own |

Houses are the backbone of your economy. Each one raises the **population cap
of its own village** and periodically **spawns new workers** (civilians or
knights, depending on your [Agriculture / Military priority](gameplay.md)).
Every village supports and raises its own people, so build houses in whichever
settlement you want to grow; the number in the HUD is the sum across all of
them. New workers are raised at whichever of your villages is **least full**
relative to its cap, so empty homes anywhere, a fresh colony's especially,
draw the growth there first. Because houses must be adjacent to what you
already own, building them is also how you expand your borders.

!!! note "Growth needs civilians"
    A house with room always raises **civilians**, so a realm cut down to a
    handful of people can work its way back. **Knights**, though, need a
    workforce behind them: with **3 or fewer civilians** houses raise workers
    only, and the fewer civilians you have the slower everything arrives. Keep
    your economy running or growth crawls.

## Bridge

| | |
| --- | --- |
| **Cost** | 3 wood |
| **Where** | Open water |

A bridge turns a water tile into walkable ground. The world already auto-spans
narrow channels with bridges at generation time, but you can build your own to
cross wider water, reach an island, or open a shortcut for your units. You can
order a whole span tile by tile right away (each order may extend a pending
one); the builder then works outward as the tiles are finished. Builders are
allowed past their usual home leash to reach a bridge site.

!!! note "Knights bridge for themselves"
    Your **knights** also lay bridges on their own: when crossing water is the
    quickest way to a foe, an enemy structure, or your rally flag (counting the
    time each span takes to lay), they bridge it tile by tile as they march.
    Each tile draws the usual **3 wood** from the stockpile
    but **no gold**, since knights are already paid for serving. The tiles they
    lay join your bridge network like any you ordered yourself. With no wood in
    the stockpile they simply can't cross.

## :lucide-pickaxe: Mine

| | |
| --- | --- |
| **Cost** | 12 stone, 45 gold (builder's pay included) |
| **Where** | Open ground within 20 tiles of a house you own |

A mine is a **bottomless stone source** that never runs out. The catch: only
**four miners** can work a single mine at once. Mines are the answer once the
natural ore around you has been depleted.

## :lucide-brick-wall: Wall

| | |
| --- | --- |
| **Cost** | 2 wood, 4 stone, 10 gold (builder's pay included) |
| **Where** | Open ground within 20 tiles of a house you own |

Walls are defensive blockers. Units path around them, so a line of walls funnels
attackers and shields your workers. Enemies can batter them down over time, so
back them up with knights.

## :lucide-tent: Hut

| | |
| --- | --- |
| **Cost** | 20 gold (builder's pay included) |
| **Where** | On a tree |

Select **Hut** mode and left-click a **tree**; a builder walks over and raises
the hut out of it. Huts **shelter civilians from raids**: workers duck inside
when danger is near, and if an occupied hut is attacked, knights rush to defend
it. Clicking a pending order again cancels it and refunds the gold.

## :lucide-map: Builder zones

| | |
| --- | --- |
| **Cost** | Free to mark; each autonomous house costs the usual 8 wood, 8 stone, 35 gold |
| **Where** | Anywhere (builds inside still follow the house rules) |

Select **Builder zone** mode and **left-click** to mark an area; clicking inside
an existing zone clears it. Zones do nothing on their own until you flip the
**"Let builders build on their own in zones"** switch in the Build section.

While the switch is on, your builders keep raising **houses** inside each zone
whenever the stockpile can afford one: the spot nearest the zone's marker goes
first, houses are kept a tile apart so the zone doesn't wall itself in, and each
zone queues **one order at a time**, so autonomous building never runs more than
one house ahead of your treasury. Flip the switch off and the ordering stops;
a pending autonomous site can be canceled like any other by clicking it in
House mode.

It's the hands-off way to grow a settlement: mark a zone, keep the gold and
timber flowing, and the village fills itself in.

## :lucide-ship: Ship goods

| | |
| --- | --- |
| **Cost** | the wood + stone you load |
| **Where** | Open water next to your village |

Trade is your gold engine. Set how much **wood** and **stone** to load in the
HUD's **Trade** section (the panel shows the projected **payout**), then pick
**Ship goods** mode and **left-click open water in your harbor** to launch a laden
cargo ship. Ships can only set sail from **near your own village**, not from any
far-off stretch of coast.

The ship charts a course **over water to the nearest allied coast** (see
**[Factions](gameplay.md#factions)**) and never sails through land. When it docks
there the goods are sold and the gold is banked. **Stone fetches more than wood**
(5 vs 2 gold per unit), so a stone-heavy hold pays best. Now and then a docking
ship also brings **[allied immigrants](gameplay.md#immigration)** home with it.

!!! warning "Pirates raid the shipping lanes"
    Rare **pirate ships** patrol the open ocean and fire cannonballs at any cargo
    ship they catch. A ship that's hit is **sunk with its cargo**, with no gold
    and no refund. See **[Units & Combat](units.md#pirates)**.

!!! note "You need a sea route to an ally"
    If there's no water path from the launch tile to any allied shore, the ship
    won't set off and your cargo stays in the hold. (Your village always sits on
    the sea, by a coast or a river that leads to it, so a route out is normally
    at hand.)

### Autonomous trade

Like [builder zones](#builder-zones), trade has a hands-off mode: flip the
**"Send cargo ships out on their own"** switch in the Trade section and the
harbor keeps dispatching laden ships without another click. Every so often it
checks the stockpile, and if it covers the configured load **in full** (and no
more than a few ships are already at sea), a ship launches from open water by
one of your villages and sails off as if you had sent it yourself.

Shipments leave from a **randomly chosen one of your coastal villages**, so
every quay in the realm sees traffic over time rather than the one port nearest
the allies carrying all of it. A colony's harbor pulls its weight the moment
the colony stands. If the village drawn has no sea route to an allied coast (a
quay on a landlocked lake, say), the realm's harbors are weighed together and
the ship leaves from whichever genuinely connects, so an unlucky draw never
stalls your trade.

Only full loads go out, so autonomous trade skims your surplus rather than
scraping the stockpile bare: set the load to what you want each shipment to
carry, and anything below that stays home. Flip the switch off and the harbor
goes quiet again. The switch is saved with the world, and autonomous shipments
run the same pirate gauntlet as manual ones.

## :lucide-globe: Settler ship

| | |
| --- | --- |
| **Cost** | 30 wood, 20 stone, 50 gold, and 3 civilians who board the ship |
| **Where** | Click the far shore to settle; the ship launches from your harbor on its own |

Settler ships are how the kingdom **expands overseas** (see
**[Settling overseas](gameplay.md#settling-overseas)**). Pick **Settler ship**
in the HUD's **Colonies** section and **left-click a distant shore**; unlike
the other modes you click the *destination*, and the expedition finds its own
launch water: it weighs every harbor you own at once and casts off from the
one with the best sea route to the landing, sailing down a river to the ocean
when that's the way out. When the order goes through, a **blue banner**
marks the landing until the ship arrives, an **alert** announces the
departure, and the Colonies section counts the expeditions at sea.

The order only goes through when everything lines up; a refusal costs nothing
and raises a toast at the click saying which of these was the problem:

- The stockpile covers the **cost**, and at least **4 civilians would stay
  home** after the crew of 3 boards, so settling can't hollow out your economy.
- The landing is **clear of every standing enemy and allied settlement**, and
  of your own too unless you click right at one of your villages: then the
  settlers **[reinforce it](gameplay.md#reinforcing-a-colony)** instead of
  founding. Colonies claim new ground, they don't crowd old.
- A **water route** reaches the shore from your harbor. A coast on a landlocked
  lake can't be sailed to, however inviting.

On landing the settlers raise the colony's **first two houses** and go to work.
From then on it's an ordinary village: houses raise its population, builds can
be ordered around it, and its coast counts as a harbor, so **cargo ships (and
warships) can launch there** too.

!!! warning "The crew sails with the hull"
    Pirates sink a settler ship with a **single hit**, and the settlers are
    lost with it; the sinking's alert offers the same one-click
    **Summon warship** as a sunk cargo ship. Time the crossing, or send the
    navy ahead.

## :lucide-anchor: Warship (the navy)

| | |
| --- | --- |
| **Cost** | 20 wood, 10 stone, 60 gold |
| **Where** | Open water next to your village |

Pick **Warship** mode in the HUD's **Navy** section and **left-click open water
in your harbor** to lay one down. The launch rules match a cargo ship's, but no
sea route is needed since a warship **patrols rather than voyages**.

Once afloat, a warship acts entirely on its own:

- **Hunts pirates.** It seeks out the nearest pirate, closes to firing range,
  and **shells it with cannonballs** until the raider sinks. A warship out-guns
  and out-lasts a lone pirate, so a small navy keeps your shipping lanes clear.
- **Bombards enemies ashore.** It also fires on enemy units on land, standing
  **off in the water** to shell the coast. It never beaches itself, so it can
  only reach foes near the shoreline.
- **Keeps its distance.** Like pirates, a warship holds a standoff off its
  target rather than ramming it.

Warships have a **hull bar** and can be **sunk by pirate fire**, so escort a
valuable convoy with more than one.

You can also pull the navy somewhere yourself with the **navy rally flag**
(see **[Units & Combat](units.md#rallying-your-warships)**), and a sunk cargo
ship's alert offers a one-click **Summon warship** that lays a new one down in
your harbor and rallies the fleet to the wreck (see
**[Controls](controls.md#alerts)**).

See **[Units & Combat](units.md#the-navy)**.

## :lucide-swords: Troop ship

| | |
| --- | --- |
| **Cost** | 25 wood, 5 stone, 20 gold, and 4 standing knights who board the ship |
| **Where** | Click the far shore to land on; the ship launches from your harbor on its own |

The military transport, for **[invading overseas](gameplay.md#invading-overseas)**.
It launches like a [settler ship](#settler-ship): pick **Troop ship** in the
HUD's **Navy** section, **left-click a distant shore**, and if the stockpile
covers the cost and a sea route reaches the landing, the **4 standing knights
nearest the harbor board** and set sail. **Levies never ship out**; they're
militia owed their pay at home. A banner marks the landing, an alert announces
the departure, and any refusal says why with a toast at the click.

Unlike settling, a troop landing may be **right by an enemy village**; that's
the point. The knights go ashore at the banner and **march on the nearest
enemy village in range on their own** (never overriding a rally flag you
planted yourself); from there their ordinary AI takes over: they fight
whoever's there, press on to the huts and walls, and capture the village the
moment it stands undefended with them inside. Once it's taken, **some of them
settle it as civilians** (see
**[After the battle](gameplay.md#after-the-battle)**).

!!! warning "The party sails with the hull"
    A troop ship is as unarmored as a cargo ship: pirates sink it with a
    **single hit**, knights and all, and the sinking's alert offers the usual
    one-click **Summon warship**. Clear the lanes or escort the crossing.

## :lucide-flag: Rally

Not a building, but it lives in the same list. See
**[Units & Combat](units.md#rallying-your-knights)** for how the rally flag
directs your knights, and
**[Rallying your warships](units.md#rallying-your-warships)** for the navy's
own flag.
