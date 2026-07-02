---
comments: true
---

# Saves & Files

Kingdom saves the entire world to a single compact **binary blob**. There's one
save slot; pressing **💾 Save** in the HUD overwrites it.

## What's saved

A save captures everything needed to reconstruct your session exactly:

- The world **seed**.
- Your **stockpile** (wood, stone, and gold).
- **Stats** — enemies defeated, units lost.
- **Every unit** — position, faction, role, and state.
- **Cargo ships** currently at sea, with their load and payout.
- **All edited chunks** — buildings, bridges, and depleted resources.

Loading replays the seed to regenerate untouched terrain, then applies your
saved edits on top, so even an "infinite" world round-trips from a small file.

## The `.dat` format

The save is a custom binary format, not JSON or a standard container:

- It begins with the magic bytes **`KGDM`** and a **version** number, so the
  loader can reject foreign or incompatible files.
- The rest is a packed representation of the state listed above.

Because the format is versioned, saves are tied to the build that wrote them;
a much newer or older build may refuse to load an incompatible file.

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
