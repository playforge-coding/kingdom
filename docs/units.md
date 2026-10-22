---
comments: true
---

# Units & Combat

You don't move units directly. Each one follows its own AI according to its
role; you influence them through [priorities, buildings, and the rally flag](controls.md).
All units navigate the tile grid with **BFS pathfinding**, routing around water,
buildings, walls, and resource nodes, or across bridges. Soldiers plan with a
**weighted search** instead: knights, who can also cut through trees and rocks
and lay bridges, weigh clearing an obstacle against walking around it (see
[Knights](#knights)), horsemen weigh leaping a stream against riding
around it (see [Horsemen](#horsemen)), and bowmen, who clear nothing, simply
walk around whatever is in the way (see [Bowmen](#bowmen)).

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

Knights are your foot soldiers, the bulk of your military. They:

- Seek out and **attack the enemy faction** wherever they find them.
- Fight in melee. Both sides take damage, and HP bars appear above wounded
  units. A knight trading blows keeps a tile off its foe, circles it, and
  dodges back and lunges in, and one badly outnumbered may lose its nerve and
  run (see [Combat resolution](#combat-resolution)).
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

## Horsemen

Horsemen are your cavalry: a knight on a horse, with a lance. Under the
**:lucide-swords: Military priority**, a house raises a horseman in place of a
knight whenever you have **fewer than one horseman for every two knights** and
the treasury holds the **50 gold** a horse costs (twice a knight), so an army
raised under Military settles at about a third mounted and tops its cavalry
back up after losses. Under Agriculture, houses raise knights only.

On the field a horseman fights like a knight, with the same blade, and answers
the same rally flag, but:

- **Rides fast and takes a beating.** A horseman moves well over half again as
  fast as a knight and has half again the hit points, so cavalry reaches a
  raided hut or a fallen village long before the foot does.
- **Charges.** A horseman with a clear, straight run at a foe **2.5 to 6
  tiles** away doesn't trot up to it: it lowers the lance and charges in at
  half again its usual pace. The impact lands **30 damage** at once (half a
  knight) and shoves the foe back nearly a tile; a foe that
  [dodges](#combat-resolution) clear takes neither. Charging again takes 6
  seconds, and once it's ready, a horseman caught in a drawn-out melee now
  and then **wheels off** to about 3.5 tiles and charges back in instead of
  dodging. Water, trees, rocks, and buildings in the way rule a charge out,
  and a charge that is blocked or whose foe gets away is called off.
- **Leaps narrow streams.** Where a stream is **no more than 2 tiles across**, a
  horseman jumps it in a straight line from one bank to the other rather than
  looking for a bridge. Rivers are carved 2-3 tiles wide (see
  [Rivers and lakes](world.md#rivers-and-lakes)), so the tributaries are open
  to cavalry while the main rivers and the lakes still bar the way. A leap is
  priced like any other step, so a bridge close at hand is still used when it's
  the quicker way. A horse never jumps from or onto a bridge, never turns in
  mid-air, and takes no part in a fight until it lands.
- **Goes around everything else.** A horseman never hacks down a tree or rock
  and never lays a bridge: whatever it can't walk or leap, it rides around, and
  a wide river stops it as surely as it stops anyone on foot.
- **Stays ashore.** [Troop ships](building.md#troop-ship) carry knights only,
  and the allies' hired [mercenaries](gameplay.md#hiring-mercenaries) are foot
  soldiers too. Cavalry fights on the home continent.
- **Keeps the saddle.** When a captured village is settled (see
  [After the battle](gameplay.md#after-the-battle)), only the soldiers on foot
  (knights and bowmen) lay down their arms; horsemen stand the garrison first.

The enemy and the allies field cavalry too, but only from villages that have
grown to **7 or more houses**, the same size at which an enemy village starts
sending [sea raids](#the-enemy-navy). So a mature enemy camp sends riders that
cross the streams your foot soldiers can't, and a tributary is no longer the
moat it was. Cavalry counts as part of a village's garrison and defenders like
any other soldier.

## Bowmen

Bowmen are your archers: foot soldiers who shoot rather than close. Under the
**:lucide-swords: Military priority**, a house raises a bowman in place of a
knight whenever you have **fewer than one bowman for every two knights** and
the treasury holds the **30 gold** a bow costs. Cavalry is topped up first
(see [Horsemen](#horsemen)), so an army raised under Military settles at about
half knights, a quarter horsemen, and a quarter bowmen. Under Agriculture,
houses raise knights only.

A bowman answers the same rally flag as everyone else and marches on the
enemy with the rest of the army, but:

- **Shoots from 5 tiles away.** The nearest foe within bowshot takes an
  **arrow every 1.5 seconds**, each one dealing **14 damage** when it lands.
  A bowman advances only until a foe is in bowshot, then stands and shoots,
  so a line of bowmen gets a couple of volleys off before the blades meet.
  Arrows fly straight and strike the first hostile
  unit they pass, so a shaft loosed at one foe may well hit another, and a
  target that steps aside is missed.
- **Shoots across water.** A foe on the far bank of a stream, or behind a
  wall, is in bowshot like any other. Only units are ever hit: an arrow does
  nothing to a hut, a wall, or a ship.
- **Gives ground but keeps shooting.** When an enemy knight or horseman gets
  within **3 tiles**, a bowman backs away from it without missing a shot,
  sidestepping along a wall or a bank that's in the way. It backs off slower
  than a knight walks, so a foe set on reaching it still does, only an arrow
  or so worse off. Archers and villagers aren't backed away from.
- **Is frail up close.** A bowman has **40 hit points** to a knight's 60 and
  no blade. Once a blade is in reach the bowman is caught: it stands and
  keeps taking shots at point-blank range but never fights back in melee, so
  a knight or a horseman that closes the distance wins one on one, though a
  lone knight does it with little to spare. Bowmen earn their keep behind
  the foot, not in front of it.
- **Never besieges.** With no foe in reach, a bowman doesn't hack at enemy
  huts and walls the way knights do; it waits for whoever comes out.
- **Goes around everything.** A bowman never hacks down a tree or rock, never
  lays a bridge, and never leaps a stream: whatever it can't walk around, it
  leaves to the knights to clear.
- **Stays ashore.** Like cavalry, bowmen never board a
  [troop ship](building.md#troop-ship).
- **Settles like the foot.** When a captured village is settled (see
  [After the battle](gameplay.md#after-the-battle)), bowmen lay down their
  bows alongside the knights.

The enemy and the allies raise bowmen too, from villages of any size, in the
same one-per-two-knights proportion. An enemy camp's archers will pick at your
knights across the stream that kept its foot at bay, and at civilians caught
out in the open, so huts matter more once the arrows start flying.

## The draft

Need soldiers *now* rather than waiting on new spawns? **Proclaim a draft** from
the HUD (see **[Controls](controls.md#proclamations)**). The moment you do:

- Every **civilian you can spare takes up arms on the spot**, converting into
  a knight at once. There is no waiting for the muster.
- Draftees are **levies** and serve **on credit**. Calling them up costs
  nothing, so a draft works even when the treasury is empty.
- A **working core of civilians is always spared**, so a draft can't hollow out
  your economy (or trip your knights into retreating home).
- The levies begin a **term of service of 4 minutes**. Through that term they
  stay under arms whatever happens, even if the realm is at peace the whole
  time, so a draft is a commitment, not a quick scare.

Proclaiming again while levies are serving calls up any civilians your houses
have raised since, and **restarts the term** for everyone serving.

Once the term is up and none of your villages has been under threat for a
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
undefended settlement won't stay yours for long, and a grown enemy village
sends [horsemen](#horsemen) among them. Enemy villages can be
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

- Fights happen at close range, about a tile apart; opposing units in melee
  **both take damage** for as long as they're within reach of each other.
- **Nobody stacks.** A soldier within 4 tiles of a foe, whether trading
  blows, closing in, chasing, or running, keeps the better part of a tile
  between itself and everyone else, friend or foe, so you can see every
  figure in a battle. A crowd that piles in shoves itself apart rather than
  standing on one tile.
- **Footwork.** A knight or horseman trading blows doesn't stand still. It
  holds its ground, **circles** its foe, or **dodges** back and then
  **lunges** in again, changing up every second or so. A wall or a bank
  turns a circling soldier back the other way. A horseman with its
  [charge](#horsemen) ready wheels off for another run instead of dodging.
- **Dodging.** Only some dodges come off (60%). A good one carries the
  soldier clear of reach and no blade touches it while it backs off; a failed
  one is caught short, still in reach, and takes the blows. Either way the
  dodger strikes nobody until it lunges back in. Arrows aren't dodged.
- **Breaking and running.** Once a second, a soldier in a fight sizes up the
  soldiers within 5 tiles. Outnumbered **three to two or worse**, it has a
  small chance (8%) of losing its nerve: it drops the fight and runs about 9
  tiles from the foes it broke from, past even a rally flag, for 4 seconds
  before turning back. It strikes nobody while it runs but can still be cut
  down or shot, and once it has run it stands its ground for a while before
  it can break again. A soldier on the run counts for neither side, so one
  runner can leave the next one even more outnumbered. An even fight never
  breaks anyone.
- [Bowmen](#bowmen) are the exception to the blade work: they **shoot from
  range**, giving ground to a blade that closes in, and deal no melee damage. An arrow hurts the
  first hostile unit it passes and nothing else. They lose their nerve like
  anyone else.
- Wounded units show an **HP bar**; when HP hits zero the unit dies.
- Kills are tallied on the HUD as **Enemies defeated**; your own deaths as
  **Units lost**.
- Walls and huts have their own HP and can be **demolished** by attackers over
  time.

## Rallying your knights

The **:lucide-flag: Rally** build mode turns a left-click into a **rally flag**:

- Your knights and horsemen **drop whatever they're doing** and rush to the
  flag.
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
