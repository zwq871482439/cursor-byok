//! Wires the Cursor-facing API routes.

pub mod bidi;
mod handlers;
pub mod inference;
mod inference_convert;
pub mod proxy;
mod run_sse;
pub mod wire_sanitize;

pub use handlers::router;
