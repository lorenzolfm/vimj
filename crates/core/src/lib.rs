//! Pure mixing logic shared by the vimj daemon, its audio engine and its
//! frontends: keys, compatibility, smart reorder, transitions and wire types.
//!
//! Nothing in here does IO.

pub mod camelot;
pub mod compat;
pub mod meta;
pub mod model;
pub mod order;
pub mod protocol;
pub mod transition;

pub use model::{Analysis, Grid, Key, Song};
pub use transition::{Preset, Transition};
