//! `evy-core` — foundational types, traits, and error type for Evy v4.
//!
//! Every other crate in the workspace depends on this one. Public surface:
//!
//! - [`Provider`] / [`ProviderKind`] — provider-agnostic dispatch trait.
//! - [`WorkerHandle`] / [`WorkerId`] / [`WorkerStatus`] — caller-facing
//!   wrapper around a dispatched worker.
//! - [`Mandate`] / [`MandateId`] — the work-order envelope.
//! - [`PolicyMode`] — Trusted / Gated / Sealed.
//! - [`Error`] / [`Result`] — the workspace error type and result alias.
//!
//! See ADR 0020 in the parent `subctl/` repo for the full architectural
//! context. Downstream crates (`evy-policy`, `evy-scheduler`,
//! `evy-providers`, …) build against this surface.

pub mod error;
pub mod mandate;
pub mod policy;
pub mod provider;
pub mod worker;

pub use error::{Error, Result};
pub use mandate::{Mandate, MandateId};
pub use policy::PolicyMode;
pub use provider::{Provider, ProviderKind};
pub use worker::{WorkerHandle, WorkerId, WorkerStatus};

#[cfg(test)]
mod object_safety {
    //! Compile-time-only assertions that the public traits stay
    //! dyn-compatible. These fns are never called; if either trait stops
    //! being object-safe the file fails to type-check.

    use super::{Provider, WorkerHandle};

    #[allow(dead_code)]
    fn assert_provider_object_safe(p: Box<dyn Provider>) -> Box<dyn Provider> {
        p
    }

    #[allow(dead_code)]
    fn assert_worker_handle_object_safe(w: Box<dyn WorkerHandle>) -> Box<dyn WorkerHandle> {
        w
    }
}
