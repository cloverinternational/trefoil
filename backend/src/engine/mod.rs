//! Port of the pinned Plexus inference engine (`packages/backend/src`):
//! unified types, per-protocol transformers, dispatch and routing. Modules
//! follow the TypeScript files they port; see `js` for value conventions.
pub mod image_rendering;
pub mod js;
pub mod openai;
pub mod sse;
pub mod transformer;
pub mod usage;
pub mod anthropic;
pub mod config;
pub mod execute;
pub mod runtime;
pub mod gemini;
pub mod responses;
pub mod routing;
pub mod dispatch;
pub mod observability;
pub mod media;
pub mod dispatcher;
