//! Port of the pinned Plexus inference engine (`packages/backend/src`):
//! unified types, per-protocol transformers, dispatch and routing. Modules
//! follow the TypeScript files they port; see `js` for value conventions.
pub mod anthropic;
pub mod config;
pub mod dispatch;
pub mod dispatcher;
pub mod execute;
pub mod gemini;
pub mod image_rendering;
pub mod js;
pub mod media;
pub mod observability;
pub mod openai;
pub mod responses;
pub mod routing;
pub mod runtime;
pub mod sse;
pub mod transformer;
pub mod usage;
