//! The interpreter loop.
//!
//! Not implemented. The plan (DESIGN.md §5.9) is a plain
//! `loop { match code[pc] { … } }` on stable Rust: no threaded dispatch, no
//! `musttail`, no unsafe, because in a shell most statements end in a syscall
//! and dispatch is a small term. The escape hatch, if it ever shows up in a
//! profile, is one closure per op.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bsh_compile::Chunk;

/// Interpreter state.
#[derive(Debug, Default)]
pub struct Vm {
    /// The chunk being executed.
    pub chunk: Chunk,
    /// Program counter.
    pub pc: usize,
}
