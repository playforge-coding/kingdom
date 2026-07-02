---
comments: true
---

# Units & Combat

You don't move units directly. Each one follows its own AI according to its
role; you influence them through [priorities, buildings, and the rally flag](controls.md).
All units navigate the tile grid with **BFS pathfinding**, routing around water,
buildings, walls, and resource nodes — or across bridges.

## Farmers

Farmers are your economy. They:

- Walk to the **nearest reachable resource** and gather it — **chopping wood**
  from trees and **mining stone** from ore, with a swing animation.
- Deposit everything into your **shared stockpile**.
- **Replant trees** and dig into **mines (caves)** once nearby resources run dry,
  so your supply doesn't collapse.
- Prefer to stay near home rather than wander off after distant resources.
- **Take shelter in huts** when a raid comes close.

Use the **Gather** toggle (Balanced / Wood / Stone) to bias what they collect.

## Knights

Knights are your military. They:

- Seek out and **attack the enemy faction** wherever they find them.
- Fight in melee — both sides take damage, and HP bars appear above wounded
  units.
- **Rush to defend** an occupied hut or a village that comes under attack.
- Follow the **rally flag** when you set one.

Lean on the **⚔ Military priority** to raise more of them when you're under
pressure.

## Enemies

The four enemy villages each field their own units. Enemy soldiers **stream out
to hunt your units**, so an undefended settlement won't stay yours for long.
Enemy villages can be **captured** the same way yours can be lost — see
[Capturing villages](gameplay.md#capturing-villages).

## Combat resolution

- Fights happen at close range; opposing units in melee **both take damage** each
  tick.
- Wounded units show an **HP bar**; when HP hits zero the unit dies.
- Kills are tallied on the HUD as **Enemies defeated**; your own deaths as
  **Units lost**.
- Walls and huts have their own HP and can be **demolished** by attackers over
  time.

## Rallying your knights

The **⚑ Rally** build mode turns a left-click into a **rally flag**:

- Your knights **drop whatever they're doing** and rush to the flag.
- The flag **clears automatically once they arrive**.
- **Right-click** (or the **✖ Clear rally** button) removes the flag manually.

Rallying is also triggered automatically when you **lose a village** — your
knights converge on it to retake the ground.
