//! Builds without agents (docs/build.md): recipes, output parsers, bounded summaries,
//! executors (this host, or a remote over ssh driven by a proxy builder), the build
//! driver, and the builder service that takes `build` jobs from the bus.

pub mod driver;
pub mod exec;
pub mod lock;
pub mod log;
pub mod parse;
pub mod recipe;
pub mod request;
pub mod service;
pub mod store;
pub mod summary;

pub use recipe::{Need, ParserKind, Recipe, RecipeError, Step};
