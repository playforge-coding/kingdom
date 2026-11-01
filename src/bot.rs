//! The computer player: runs a realm the way a person would, by looking at
//! the game and giving the same orders the HUD gives. It owns no state of
//! its own and touches nothing directly, so a seat can pass between a person
//! and the computer at any moment, and a computer-run realm can do nothing a
//! player couldn't.
//!
//! The player realm's own automation already does most of the work once it
//! is switched on (builders raising houses in zones, the harbor sending
//! cargo ships, alarms rallying the knights, levies standing down). What is
//! left for the bot is what a player decides by hand: where to zone, what to
//! prioritize, when to recruit and arm, when to attack, and when to answer a
//! proposal.

use glam::Vec2;

use crate::game::{
    AUTO_ZONE_RADIUS, Command, Faction, Game, GatherPriority, Priority, RealmId, Relation, Rng,
    SiteKind, Tile,
};

/// Seconds between a computer-run realm's turns to think. Long enough that
/// its orders look deliberate rather than twitchy, and that a dozen such
/// realms cost the simulation little.
pub const THINK_INTERVAL: f32 = 4.0;

/// Soldiers a realm keeps at home before it sends the rest at a foe.
const GARRISON: usize = 4;
/// Soldiers beyond the garrison that make an attack worth mounting.
const STRIKE_FORCE: usize = 6;
/// How far from home the bot sends its soldiers, in tiles.
const STRIKE_RANGE: f32 = 70.0;
/// Civilians a realm wants at work before it shifts to raising soldiers.
const WORKFORCE: usize = 10;

/// What `realm` orders this turn. Pure: reads the game and returns the
/// orders for `Game::apply`; `rng` only breaks ties.
pub fn think(game: &Game, realm: RealmId, rng: &mut Rng) -> Vec<Command> {
    let mut orders = Vec::new();
    let me = game.realm(realm);
    let faction = Faction::Realm(realm);
    let houses = game.houses(realm);
    if houses.is_empty() {
        return orders;
    }
    let center = game.start_center(realm);

    // The economy runs itself once switched on: builders raise houses in a
    // zone over the capital, and the harbor ships whatever the configured
    // load covers. A zone is marked once; the switch stays on.
    if !me.auto_build {
        orders.push(Command::SetAutoBuild(true));
    }
    if me.auto_zones().is_empty() {
        orders.push(Command::ToggleZone(tile_of(center)));
    } else if zone_is_full(game, realm) && me.auto_zones().len() < 6 {
        // Every spot in the standing zones is taken: open another one a
        // zone's width out from a random house, so the realm keeps growing.
        let (hx, hy) = houses[rng.range(0, houses.len() as i32) as usize];
        let (dx, dy) = [(1, 0), (-1, 0), (0, 1), (0, -1)][rng.range(0, 4) as usize];
        let span = AUTO_ZONE_RADIUS * 2 + 1;
        orders.push(Command::ToggleZone((hx + dx * span, hy + dy * span)));
    }
    if !me.auto_trade {
        orders.push(Command::SetAutoTrade(true));
    }

    // Gather whichever of wood and stone the stockpile is shorter of.
    let gather = if me.wood + 10 < me.stone {
        GatherPriority::Wood
    } else if me.stone + 10 < me.wood {
        GatherPriority::Stone
    } else {
        GatherPriority::Balanced
    };
    if gather != me.gather_priority {
        orders.push(Command::SetGatherPriority(gather));
    }

    // People first until the workforce stands, then soldiers; and soldiers
    // whenever a village of ours is under attack.
    let threatened = game
        .villages()
        .iter()
        .any(|v| v.faction == faction && v.threatened);
    let civilians = game
        .entities
        .iter()
        .filter(|e| e.faction == faction && !e.is_soldier())
        .count();
    let soldiers = game.population(realm) - civilians;
    let priority = if threatened || civilians >= WORKFORCE {
        Priority::Military
    } else {
        Priority::Agriculture
    };
    if priority != me.priority {
        orders.push(Command::SetPriority(priority));
    }
    // A village under attack with next to nobody under arms calls the draft.
    if threatened && soldiers < 2 && me.levy_term_remaining().is_none() {
        orders.push(Command::ProclaimDraft);
    }

    // Huts shelter the civilians: one near the capital while there is a tree
    // to raise it from and nothing else pending.
    if game.realm_huts(realm) == 0
        && game.site_of(realm, SiteKind::Hut).is_none()
        && let Some(tree) = game.tree_near(tile_of(center), 10)
    {
        orders.push(Command::Order {
            tile: tree,
            kind: SiteKind::Hut,
        });
    }

    // With a strike force to spare and nothing to defend, march on the
    // nearest hostile village within range. One flag at a time: the
    // soldiers share it out and it lifts when they arrive.
    if !threatened && soldiers >= GARRISON + STRIKE_FORCE && me.rallies().is_empty() {
        let target = game
            .villages()
            .iter()
            .filter(|v| game.diplomacy().hostile(v.faction, faction))
            .map(|v| v.center)
            .filter(|&c| c.distance(center) <= STRIKE_RANGE)
            .min_by(|a, b| a.distance(center).total_cmp(&b.distance(center)));
        if let Some(t) = target {
            orders.push(Command::ToggleRally(tile_of(t)));
        }
    }

    // Diplomacy: when alliances are allowed, make peace with the realm that
    // is pressing hardest on everyone (the biggest), one proposal standing
    // at a time, and answer any offer by accepting it.
    if game.diplomacy().ai_may_ally {
        for other in 0..game.realm_count() {
            if other == realm {
                continue;
            }
            if game.diplomacy().relation(realm, other) == Relation::Offered {
                orders.push(Command::AcceptAlliance(other));
            }
        }
        let proposing = (0..game.realm_count())
            .any(|o| o != realm && game.diplomacy().relation(realm, o) == Relation::Proposed);
        if !proposing {
            let biggest = (0..game.realm_count())
                .filter(|&o| o != realm && game.diplomacy().relation(realm, o) == Relation::War)
                .max_by_key(|&o| game.population(o));
            if let Some(o) = biggest
                && game.population(o) > game.population(realm)
            {
                orders.push(Command::ProposeAlliance(o));
            }
        }
    }

    orders
}

/// Can no house go anywhere in any of the realm's builder zones?
fn zone_is_full(game: &Game, realm: RealmId) -> bool {
    game.realm(realm).auto_zones().iter().all(|&(zx, zy)| {
        !(-AUTO_ZONE_RADIUS..=AUTO_ZONE_RADIUS).any(|dy| {
            (-AUTO_ZONE_RADIUS..=AUTO_ZONE_RADIUS)
                .any(|dx| game.house_may_go(realm, (zx + dx, zy + dy)))
        })
    })
}

fn tile_of(p: Vec2) -> Tile {
    (p.x.floor() as i32, p.y.floor() as i32)
}
