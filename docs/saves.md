---
comments: true
---

# Saves & Files

Kingdom saves the entire world to a compact **binary blob**. Saves live in
named **slots**: type a name next to **:lucide-save: Save** in the HUD and the
world is stored under it. Saving under a name you've used before overwrites
that slot; saving under a new name adds one. There is no fixed slot count, so
you can keep as many worlds as your storage holds.

Every slot appears on the main menu under **Saved Worlds**, each with a
**Load** and a **Delete** button. Delete asks you to click a second time
before it removes anything.

A loaded or freshly created world prefills the save box (with the slot it came
from, or a name built from the seed), so a plain **Save** keeps updating the
same slot unless you rename it. Slot names are trimmed to letters, digits,
spaces, dashes and underscores, and capped at 40 characters, so they double as
file names.

## What's saved

A save captures everything needed to reconstruct your session exactly:

- The world **seed**.
- Your **stockpile** (wood, stone, and gold).
- **Stats:** enemies defeated and units lost.
- **Every unit:** position, faction, role, and state, including where settlers
  on the road between villages are headed, so they finish the trip after a
  load.
- **Cargo ships** currently at sea, with their load and payout, plus the
  **autonomous-trade switch** and the configured cargo load.
- **Settler and troop ships** under sail, with everyone aboard, their
  destination, and their owner (yours, a hired allied company's, or an enemy
  raid's).
- **Warships** afloat, with their owner (yours, a hired allied escort, or the
  enemy's), their remaining hull, any standing orders (an escort's crossing
  or an enemy ship's landing), and how long an enemy ship has been out on its
  sortie.
- **Construction orders** in progress, your **builder zones**, and the
  **autonomous-building switch**.
- **All edited chunks:** buildings, bridges, and depleted resources.

Rally flags (the knights' and the navy's) are deliberately transient: they
direct a moment, not a realm, so a loaded game starts with none planted. The
levies' term of service is transient too: serving levies are saved and still
owed their pay, but a loaded game's levies are free to stand down as soon as
the realm has been quiet for a little while.

The **[testing ground](testing.md)** is never saved. It has no **Save**
button, so experimenting there can't fill a save slot with an empty grid.

Loading replays the seed to regenerate untouched terrain, then applies your
saved edits on top, so even an "infinite" world round-trips from a small file.
Loading also tidies up after older saves that stored unedited terrain: any
chunk that matches what the seed regenerates is treated as untouched, so the
next save you write slims back down to just your edits.

## The `.dat` format

The save is a custom binary format, not JSON or a standard container:

- It begins with the magic bytes **`KGDM`** and a **version** number, so the
  loader can reject foreign files and knows which revision of the format it is
  reading.
- The rest is a packed representation of the state listed above.

The format is versioned so it can grow without stranding anyone: a newer build
keeps loading the saves older supported versions wrote, filling in whatever
they predate with sensible defaults (a save from before settler ships simply
loads with none at sea, and one from before the enemy and allied navies loads
every warship as yours). Only a save written by a *newer* build than the one
loading it, or one older than the loader still supports, is refused.

## Where they live

=== "Native"

    Each slot is a file named after it, **`<name>.dat`**, in a `saves/`
    directory under the platform's data dir (for example
    `~/Library/Application Support/kingdom/saves` on macOS,
    `~/.local/share/kingdom/saves` on Linux, `%APPDATA%\kingdom\saves` on
    Windows). A save file from before slots existed (`kingdom_save.dat`) is
    moved into the directory on startup as a slot named **world**.

=== "Web"

    The browser can't write arbitrary files, so each slot's blob is stored as
    its own record in **IndexedDB** under your site's origin, keyed by the
    slot's name (the pre-slot save was keyed **world**, so it shows up as a
    slot of that name). Saves persist across page reloads as long as you
    don't clear site data.

In both cases the menu's **Saved Worlds** list shows whatever slots are
present, and nothing when there are none.
