//! The Kingdom simulation, shared by the game client and the match server.
//! Everything here runs headless: the world and its terrain, the realms and
//! their units, the orders a realm can give, and the computer player that
//! gives them. Drawing, input, saving to disk and the network live in the
//! crates that use this one.

pub mod bot;
pub mod game;
pub mod grid;
pub mod net;
pub mod pathfind;
pub mod world;
