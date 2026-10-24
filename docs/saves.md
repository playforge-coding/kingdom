---
comments: true
---

# Saves & Files

Kingdom saves the entire world to a single compact **binary blob**. There's one
save slot; pressing **:lucide-save: Save** in the HUD overwrites it.

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
button, so experimenting there can't overwrite your one save slot with an
empty grid.

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

## Where it lives

=== "Native"

    Saved to a file named **`kingdom_save.dat`**. **Load Saved World** on the
    menu reads it back.

=== "Web"

    The browser can't write arbitrary files, so the same blob is stored in
    **IndexedDB** under your site's origin. It persists across page reloads as
    long as you don't clear site data.

In both cases the **Load Saved World** menu button is enabled only when a save
is present.
