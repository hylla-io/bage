//! Båge — a hash-anchored structural file editor (Rust implementation).
//!
//! Region-anchored, ID-blind editing over any text file: tree-sitter CST
//! parsing with a lossless text fallback, a two-hash drift discipline
//! (raw/normalized xxHash64), a durable WAL for crash recovery, and atomic
//! file writes. Byte-identical hashing and normalization with the Go
//! implementation and Hylla.

pub mod atomicwrite;
pub mod edit;
pub mod format;
pub mod hashing;
pub mod inspect;
pub mod lsp;
pub mod normalize;
pub mod parser;
pub mod region;
pub mod render;
pub mod session;
pub mod wal;
