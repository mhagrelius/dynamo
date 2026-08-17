//! Three Siemens Inhab energy monitors — white-labelled Emporia Vue 3s — read
//! out of a cloud that keeps a week of minutes, into a database that keeps all
//! of them.
//!
//! The split is the one the sibling apps keep. `scale`, `plan` and `emporia`
//! are pure: what to ask for, what a URL looks like, what a body means. `http`
//! is the only file that opens a socket and `store` the only one that talks to
//! Postgres. Everything worth arguing about is on the pure side, where it has
//! tests instead of a comment saying it was thought about.

pub mod cognito;
pub mod config;
pub mod emporia;
pub mod http;
pub mod plan;
pub mod scale;
pub mod store;
