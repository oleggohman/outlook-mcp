//! `hxprobe` library: parsers for `HxStore.hxd`, the New Outlook message store.
//!
//! The command-line program in `main.rs` is a thin layer over these modules.

pub mod block;
pub mod export;
pub mod header;
pub mod inspect;
pub mod item;
pub mod kv;
mod le;
pub mod lz;
pub mod names;
pub mod objects;
pub mod props;
pub mod props_gen;
pub mod record;
pub mod schema;
pub mod value;

#[cfg(test)]
pub mod testutil;
