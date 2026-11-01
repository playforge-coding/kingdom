---
comments: true
---

# Multiplayer

Kingdom is growing a multiplayer mode: several empires on one map, each run
by a person or by the computer, playing by the same rules the single-player
game uses today. This page describes the design and how far along it is.
Nothing here changes how a single-player world plays.

## The design

- **Every empire is a realm.** A realm is an empire run on the player's
  rules: it gathers, builds, trades, recruits and hires exactly as you do. The
  world holds any number of them. The computer's rival camps (the enemy) and
  the trade-partner villages (the allies) stay what they are, non-realm
  factions with their own behavior.
- **A match has a fixed number of seats.** People take seats; the computer
  fills any seat nobody has taken, so a match with every seat taken has no
  computer-run empire at all. The computer plays by issuing the same orders a
  person would, which also means a seat whose player drops out keeps running
  until they come back. There is a practical cap on seats (`MAX_SLOTS`), but
  nothing is sized by it: raising the one constant is the whole change.
- **Players choose their alliances.** Realms start at war with one another.
  An alliance needs both sides to agree; leaving one takes effect after a
  short notice. Whether computer-run realms may make alliances is up to the
  server.
- **One server, many rooms.** A server hosts rooms. Anyone may create a room
  (if the server allows it) and others join it; each room is one match with
  its own settings, chosen by its creator within the limits the server
  operator sets: how many seats, whether the computer's realms may ally, and
  whether players may join a match already in progress.
- **The server runs the game.** Clients send orders and receive the state;
  the simulation runs in one place, so there is nothing to keep in lockstep.

## Where it stands

The work lands in stages, each of which keeps single-player working:

```mermaid
flowchart LR
    A[Realms] --> B[Orders]
    B --> C[Diplomacy]
    C --> D[Match setup]
    D --> E[Computer players]
    E --> F[Server and rooms]
```

| Stage | What it does | Status |
| --- | --- | --- |
| **Realms** | Every per-player thing (stockpile, flags, switches, alerts, hired help) lives in a `Realm`, and the game holds a list of them. Houses, ships and orders carry their owner. | Done |
| **Orders** | Everything the HUD does is a `Command` the game applies for a given realm (`Game::apply`), so a remote player and the computer can give the same orders a local one does. The HUD only reads the game and hands orders back. | Done |
| **Diplomacy** | A relations table (`Diplomacy`) decides who fights whom. Realms start at war; `ProposeAlliance`, `AcceptAlliance`, `DeclineAlliance` and `LeaveAlliance` orders change that, with a one-minute notice before an alliance ends. The Realm panel shows and drives it. A computer-run realm answers a proposal at once, accepting when the match allows it; weighing the offer properly comes with the computer players. | Done |
| **Match setup** | `MatchSettings` (seats, whether the computer's realms may ally) and `Game::new_match`, which plants one capital per seat around a ring, well apart, with the first seat a person's and the rest the computer's. The main menu offers the setting. Each realm past the first gets a hue of its own, and the atlas recolors every team-colored sprite for it. | Done |
| **Computer players** | A player that runs a realm through the same orders a person uses. | Planned |
| **Server and rooms** | A headless server running matches in rooms over WebSockets, and the client's lobby and connection screens. | Planned |

## What this means for saves

A save now records every realm along with who owns each ship, construction
order and hired unit (format version 20), and the state of diplomacy between
them (version 21). Older saves describe one realm and load as one, at war
with nobody but the enemy; see **[Saves & Files](saves.md)**.
