//! String processing with file/line/col information and the regular Rust `str` API.
//!
//! Vendored from the `spanned` crate, which depends on an older `annotate-snippets`.

#![allow(missing_docs)]

mod error;
mod span;

pub use error::*;
pub use span::*;

/// Result with a spanned error.
pub type Result<T, E = Error> = std::result::Result<T, E>;
