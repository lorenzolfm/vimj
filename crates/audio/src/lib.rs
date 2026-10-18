//! vimj's audio side: decoding through ffmpeg, musical analysis, and the
//! two-deck engine that plays a set with its transitions.

pub mod analysis;
pub mod deck;
pub mod dsp;
pub mod engine;
pub mod pcm;
pub mod player;
