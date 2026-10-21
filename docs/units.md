---
comments: true
---

# Units & Combat

You don't move units directly. Each one follows its own AI according to its
role; you influence them through [priorities, buildings, and the rally flag](controls.md).
All units navigate the tile grid with **BFS pathfinding**, routing around water,
buildings, walls, and resource nodes, or across bridges.

## Civilians

Civilians are your economy. Each one takes up whichever job the realm needs at
the moment, and the HUD's status line shows how many are working each:

- **Farmers** walk to the **nearest reachable resource** and gather it,
  **chopping wood** from trees and quarrying **stone** from ore with a swing
  animation, and **replant trees** once the nearby pickings run dry.
- **Miners** work your **[mines (caves)](building.md#mine)**: bottomless stone,
  but only four to a mine.
- **Builders** claim pending **[construction orders](building.md)**, walk to the
  site, and put the structure up over a few real seconds. The **builder's pay**
  is part of every order's gold cost.

Whatever the job, civilians:

- Deposit everything they gather into your **shared stockpile**.
- Prefer to stay near home rather than wander off after distant resources
  (builders may step out of that leash to reach a site, such as a bridge span).
- **Take shelter in huts** when a raid comes close.

Use the **Gather** toggle (Balanced / Wood / Stone) to bias what they collect.
A construction order outranks everything else: even a miner puts down the pick
when a site is waiting for hands.

## Knights

Knights are your military. They:

- Seek out and **attack the enemy faction** wherever they find them.
- Fight in melee. Both sides take damage, and HP bars appear above wounded
  units.
- **Press the assault on enemy settlements** once the field is clear: with no
  foe left to chase, knights march on the nearest enemy **huts and walls** and
  hack them down rather than idling at home.
- **Rush to defend** an occupied hut or a village that comes under attack.
- **Bridge their own way across water.** When the only route to a foe, an enemy
  structure, or your rally flag crosses water, knights lay the bridges
  themselves as they go: each tile costs **3 wood** but **no gold**, since
  they're already paid for serving.
- Follow the **rally flag** when you set one.

Lean on the **:lucide-swords: Military priority** to raise more of them when you're under
pressure.

## The draft

Need soldiers *now* rather than waiting on new spawns? **Proclaim a draft** from
the HUD (see **[Controls](controls.md#proclamations)**). For a short spell:

- Your **civilians may spontaneously take up arms**, converting into knights at
  random over the course of the draft.
- Draftees are **levies** and serve **on credit**. Calling them up costs
  nothing, so a draft works even when the treasury is empty.
- A **working core of civilians is always spared**, so a draft can't hollow out
  your economy (or trip your knights into retreating home).

Once the draft has ended and none of your villages has been under threat for a
little while, the invasion counts as repelled and the levies **stand down**:

- Each surviving levy is **paid 15 gold** and **goes back to civilian work**,
  keeping any wounds from the fight. Levies who fell in battle are owed nothing.
- If the treasury can't cover everyone, the levies you can't pay **desert** and
  are gone for good.
- An alert tells you the levies were paid, or that some deserted.

While levies are serving, the Proclamations panel shows how many there are and
how much you owe. Proclaiming another draft, or a fresh raid on one of your
villages, keeps them under arms a while longer.

It's a burst of military muscle when a raid is bearing down, at the price of a
thinner workforce during the fight and a bill once it's won. Keep the trade
routes running so the gold is there when it comes due.

## Enemies

The enemy villages each field their own units, and **more are founded over time**
as the map expands. Enemy soldiers **stream out to hunt your units**, so an
undefended settlement won't stay yours for long. Enemy villages can be
**captured** the same way yours can be lost (see
[Capturing villages](gameplay.md#capturing-villages)).

## Pirates

Out on the **open ocean** lurk rare **pirate ships**, a hazard to your trade
rather than a faction you fight on land. They:

- **Keep to the open sea.** Pirates never enter rivers or lakes, so they can't
  reach inland water or come ashore. They haunt the deep water your shipping
  lanes cross.
- **Hunt your vessels.** A pirate that spots one of your
  [cargo ships](building.md#ship-goods), [settler ships](building.md#settler-ship),
  or [warships](building.md#warship-the-navy)
  gives chase and, once in range, opens fire with **cannonballs**. It holds a
  short **standoff** and shells from there rather than piling onto the hull.
- **Sink cargo on a hit.** A cannonball that strikes a cargo ship **sinks it,
  cargo and all**. The goods are lost at sea and no gold is paid. A settler
  ship fares the same, and worse: its **crew is lost with it**, counted among
  your casualties. Warships are armored and instead lose **hull** with each
  hit, sinking only when it runs out.

They're uncommon, but a laden ship is a tempting target. Losing a stone-heavy
hold to pirates stings, so time your shipments or send a warship escort.

When pirates do sink a cargo ship, the **alert** it raises carries a one-click
**Summon warship** button whenever you can afford one: it lays a fresh warship
down in your harbor and **rallies the whole navy to the wreck**. See
**[Alerts](controls.md#alerts)**.

## The navy

Build a **[warship](building.md#warship-the-navy)** and the player gains a
seafaring military that fights on its own:

- **Attacks pirates.** A warship hunts the nearest pirate, closes to a standoff,
  and **shells it with cannonballs**. It's tougher and quicker-firing than a
  pirate, so it wins a straight duel and keeps the shipping lanes clear.
- **Shells enemies ashore.** It also fires on enemy land units, holding position
  **out in the water**. It never leaves the sea, so it bombards the coast from a
  distance and can't reach foes far inland.
- **Makes for the open ocean when idle.** With no quarry in sight, a warship
  sails out to the **deep sea** and patrols there, so it's already on the shipping
  lanes when the next raider appears.
- **Answers the navy rally flag.** A planted flag outranks the hunt and the
  patrol alike; see **[Rallying your warships](#rallying-your-warships)**.
- **Has a hull bar** and can be **sunk by pirate fire**; pair up warships against
  a pack of raiders.

## Combat resolution

- Fights happen at close range; opposing units in melee **both take damage** each
  tick.
- Wounded units show an **HP bar**; when HP hits zero the unit dies.
- Kills are tallied on the HUD as **Enemies defeated**; your own deaths as
  **Units lost**.
- Walls and huts have their own HP and can be **demolished** by attackers over
  time.

## Rallying your knights

The **:lucide-flag: Rally** build mode turns a left-click into a **rally flag**:

- Your knights **drop whatever they're doing** and rush to the flag.
- The flag **clears automatically once they arrive**.
- **Right-click** (or the **:lucide-x: Clear rally** button) removes the flag manually.

Rallying is also triggered automatically when you **lose a village** or a
**hut comes under attack**: the knights **near it** converge to retake the
ground, while those stationed farther away hold their own posts. Only a flag
you plant yourself calls every knight in the realm, and an automatic alarm
with no knight close enough to answer simply lifts itself.

## Rallying your warships

The navy has a flag of its own. The **:lucide-flag: Rally warships** mode in
the HUD's **Navy** section turns a left-click on **open water** into a navy
rally flag, drawn in your faction's blue:

- Every warship **drops its hunt or patrol** and sails for the flag, though its
  **guns stay hot**: it still fires on any pirate or shore enemy that comes
  into range along the way.
- The flag **lifts once one warship arrives**, and the fleet goes back to
  hunting and patrolling on its own, right where you pointed it.
- **Right-click** (or the **:lucide-x: Clear navy rally** button) removes it
  manually. Clicking the flagged tile again also lifts it.

The **Summon warship** button on a sunk-cargo [alert](controls.md#alerts)
uses this same flag: it plants one at the wreck, so the new hull and the rest
of the navy converge on the raiders.
