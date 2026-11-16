//! Proof that send confirmation came from the interactive Fell UI.
//!
//! `fell-mcp` must not depend on this crate. The GPUI app (`fell`) is the
//! intended consumer. [`InteractiveUi`] is not a unit struct and is not
//! [`Default`], so a crate that only depends on `fell-core` cannot mint a
//! confirmation via type inference.

#![forbid(unsafe_code)]

mod sealed {
    pub trait Sealed {}
}

/// Token proving the caller is UI code that may mint a send confirmation.
///
/// The private field blocks `InteractiveUi` / `InteractiveUi {}` construction
/// from other crates. Only [`InteractiveUi::for_fell_app`] builds one; that
/// lives here so `fell` can call it and `fell-mcp` cannot without taking this
/// dependency (which it must not).
pub struct InteractiveUi {
    _private: (),
}

impl sealed::Sealed for InteractiveUi {}

/// Marker implemented only for [`InteractiveUi`].
pub trait UiProof: sealed::Sealed {}

impl UiProof for InteractiveUi {}

impl InteractiveUi {
    /// Constructed by the GPUI app (`fell`) after an explicit Send click.
    pub const fn for_fell_app() -> Self {
        Self { _private: () }
    }
}
