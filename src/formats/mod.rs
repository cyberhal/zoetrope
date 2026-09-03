//! Provider-owned wire decoders.
//!
//! Raw record DTOs stay private here; the rest of the crate consumes only
//! [`crate::event::SessionEvent`].

pub mod claude;
pub mod codex;
