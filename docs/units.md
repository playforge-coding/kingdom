---
comments: true
---

# Units & Combat

You don't move units directly. Each one follows its own AI according to its
role; you influence them through [priorities, buildings, and the rally flag](controls.md).
All units navigate the tile grid with **BFS pathfinding**, routing around water,
buildings, walls, and resource nodes, or across bridges. Knights, who can also
cut through trees and rocks and lay bridges, plan with a **weighted search**
instead, so that clearing an obstacle is weighed against walking around it (see
[Knights](#knights)).

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
- **Go around before cutting through.** A knight can hack down a tree or rock
  in its way, but it takes a while and yields nothing, so its route planner
  prices every obstacle at the distance it could have walked in that time and
  only cuts through when that's genuinely quicker than any way around. A route
  planned over clear ground is never hacked at: if a sapling matures on it, the
  knight plans afresh and walks around.
- **Bridge their own way across water.** When bridging to a foe, an enemy
  structure, or your rally flag is quicker than any way around (or there is no
  way around at all), knights lay the bridges themselves as they go: each tile
  costs **3 wood** but **no gold**, since they're already paid for serving.
  Bridging is priced like hacking, by the time it takes, so a flag planted just
  off the shore is reached from the bank, not bridged to.
- **Ship out overseas.** Where no bridge can reach, a
  [troop ship](building.md#troop-ship) carries a party of standing knights to a
  far shore (levies stay home). On landing they march on the nearest enemy
  village within range by themselves; ashore, they fight exactly as they do at
  home.
- **Settle what they conquer.** When a village is captured, some of the knights
  standing in it lay down their arms and become civilians; on quiet ground all
  but a single garrison do (see
  [After the battle](gameplay.md#after-the-battle)).
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
[Capturing villages](gameplay.md#capturing-villages)). Once an enemy village
has grown large, it also sends **raids by sea** against your coasts; see
**[The enemy navy](#the-enemy-navy)**.

## Pirates

Out on the **open ocean** lurk rare **pirate ships**, a hazard to your trade
rather than a faction you fight on land. They:

- **Keep to the open sea.** Pirates never enter rivers or lakes, so they can't
  reach inland water or come ashore. They haunt the deep water your shipping
  lanes cross.
- **Hunt any hull.** A pirate that spots a ship, whoever's flag it flies,
  gives chase and, once in range, opens fire with **cannonballs**. It holds a
  short **standoff** and shells from there rather than piling onto the hull.
  Your [cargo ships](building.md#ship-goods), [settler ships](building.md#settler-ship),
  [troop ships](building.md#troop-ship), and [warships](building.md#warship-the-navy)
  are all fair game, and so are the allies' and the enemy's ships; a pirate
  that sinks an enemy raid before it lands has done you a favor.
- **Sink cargo on a hit.** A cannonball that strikes a cargo ship **sinks it,
  cargo and all**. The goods are lost at sea and no gold is paid. Settler and
  troop ships fare the same, and worse: **everyone aboard is lost with the
  hull**, counted among your casualties. Warships are armored and instead lose
  **hull** with each hit, sinking only when it runs out.

They're uncommon, but a laden ship is a tempting target. Losing a stone-heavy
hold to pirates stings, so time your shipments or send a warship escort.

When a cargo ship is sunk, by pirates or by enemy guns, the **alert** it raises
carries a one-click **Summon warship** button whenever you can afford one: it
lays a fresh warship down in your harbor and **rallies the whole navy to the
wreck**. See **[Alerts](controls.md#alerts)**.

## The navy

Build a **[warship](building.md#warship-the-navy)** and the player gains a
seafaring military that fights on its own:

- **Attacks pirates.** A warship hunts the nearest pirate, closes to a standoff,
  and **shells it with cannonballs**. It's tougher and quicker-firing than a
  pirate, so it wins a straight duel and keeps the shipping lanes clear.
- **Hunts the enemy's ships.** An [enemy troop ship or warship](#the-enemy-navy)
  in sight is hunted the same way. A troop ship sinks from a single hit,
  raiders and all; an enemy warship has a weaker hull than yours, so a
  one-on-one duel goes to the navy.
- **Shells enemies ashore.** It also fires on enemy land units, holding position
  **out in the water**. It never leaves the sea, so it bombards the coast from a
  distance and can't reach foes far inland.
- **Makes for the open ocean when idle.** With no quarry in sight, a warship
  sails out to the **deep sea** and patrols there, so it's already on the shipping
  lanes when the next raider appears.
- **Answers the navy rally flag.** A planted flag outranks the hunt and the
  patrol alike; see **[Rallying your warships](#rallying-your-warships)**.
- **Has a hull bar** and can be **sunk by hostile fire**, a pirate's or an
  enemy warship's; pair up warships against a pack of raiders.

The allies will **hire out a warship** to sail with the navy; see
**[Hiring an escort](gameplay.md#hiring-an-escort)**. A hired escort flies the
allies' green hull but behaves as one of your own, flag and all.

## The enemy navy

The sea stops being yours alone once the enemy's villages grow. An enemy
village of **7 or more houses** on a river or coast, with knights to spare
beyond a garrison, sends **sea raids**:

- **A troop ship of 4 knights** sails for the coast by your **least-defended
  village**, the one with the fewest knights standing in it, so a fresh colony
  or a stripped capital is exactly what it goes for. A red-hulled ship and a
  **red banner on the landing** mark the crossing, the Navy section of the HUD
  counts the enemy fleet at sea, and an alert at the landing says a fleet has
  set sail. That alert carries a one-click **Rally navy** button whenever you
  have a warship afloat, planting the navy flag at the landing.
- **Ashore, the raiders are ordinary enemy knights**: they hunt your units, and
  an undefended village with one of them inside is
  [lost](gameplay.md#capturing-villages).
- **A warship escorts the raid** once the village has grown to **9 houses**.
  It sails the same route, hunts your ships and shells your coast like your
  own navy does, and then **holds station off the landing**, firing on
  whatever comes within reach, until its sortie runs out some minutes later
  and it withdraws. At most two enemy warships are out at once, and only one
  troop ship.

Enemy hulls are as sinkable as anyone's: your warships, a hired escort, and
even pirates will send them down, and an alert says so. The raids can't
start until an enemy camp has had time to grow, so an early push against the
enemy's coastal villages keeps the sea quiet.

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
  **guns stay hot**: it still fires on any pirate, enemy ship, or shore enemy
  that comes into range along the way. Hired allied escorts answer the flag
  too.
- The flag **lifts once one warship arrives**, and the fleet goes back to
  hunting and patrolling on its own, right where you pointed it.
- **Right-click** (or the **:lucide-x: Clear navy rally** button) removes it
  manually. Clicking the flagged tile again also lifts it.

The **Summon warship** button on a sunk-cargo [alert](controls.md#alerts)
uses this same flag: it plants one at the wreck, so the new hull and the rest
of the navy converge on the raiders.
