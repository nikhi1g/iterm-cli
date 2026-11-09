//! `iterm`: drive iTerm2 from code, for humans and agents alike. iTerm2's CLI (`<noun> <verb> [SELECTOR]
//! [PLACEMENT] [GEOMETRY] [MULTIPLICITY] [PAYLOAD]`) over an [`app::App`]: the iTerm2 adapter ([`iterm2`]) or a
//! test's fake. One connection and one snapshot per command.

pub mod action;
pub mod agent;
pub mod agents;
pub mod hook;
pub mod app;
pub mod args;
pub mod board;
pub mod claude;
pub mod cli;
pub mod conn;
pub mod content;
pub mod exec;
pub mod fswait;
pub mod geometry;
pub mod grammar;
pub mod guide;
pub mod iterm2;
pub mod land;
pub mod mail;
pub mod named;
pub mod out;
pub mod plan;
pub mod select;
pub mod settings;
pub mod store;
pub mod subagent;
pub mod tree;
pub mod ui;
pub mod view;
pub mod watch;
