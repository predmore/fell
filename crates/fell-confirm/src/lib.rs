//! Proof that send confirmation came from the interactive Fell UI.
//!
//! `fell-mcp` must not depend on this crate. The GPUI app (`fell`) is the
//! intended consumer. [`InteractiveUi`] is a ZST anyone who takes this
//! dependency can construct; the boundary is Cargo, not a runtime check.

#![forbid(unsafe_code)]

/// Token proving the caller is UI code that may mint a send confirmation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InteractiveUi;

impl InteractiveUi {
    pub const fn new() -> Self {
        Self
    }
}
