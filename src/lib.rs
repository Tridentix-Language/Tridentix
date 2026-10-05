//! Library crate root — exposes Tridentix's compiler/interpreter modules so
//! other binaries in this workspace (`pkg` the package-manager CLI,
//! `tridentix-lsp` the language server) can reuse the exact same lexer,
//! parser, typechecker, and interpreter as the main `tridentix` binary,
//! rather than duplicating or re-implementing any of it.
//!
//! Note: the native actor runtime (`extern "C"` spawn/send/shutdown
//! primitives AOT-compiled code calls into) lives in the SEPARATE
//! `actor_rt/` crate, not here — deliberately, so its static lib stays
//! tiny and LLVM-independent (see `actor_rt/Cargo.toml`'s doc comment
//! for the real linker problem that split fixes).

pub mod ast;
pub mod codegen;
pub mod interpreter;
pub mod lexer;
pub mod parser;
pub mod stdlib;
pub mod typechecker;
