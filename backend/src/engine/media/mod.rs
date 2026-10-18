//! Port of Plexus media dispatch: `services/dispatch/media-dispatcher.ts`,
//! `embeddings-transformer-factory.ts`, `image-transformer-factory.ts`,
//! `image-model-bridge.ts` and the embeddings / image / speech /
//! transcriptions transformers they use.
//!
//! Module ↔ TypeScript file:
//! - `dispatcher` — `media-dispatcher.ts` (+ the `Dispatcher` host helpers).
//! - `image` — `transformers/image.ts`, `transformers/images/*`, the image factory.
//! - `transformers` — embeddings, speech and transcriptions transformers.
//! - `bridge` — `image-model-bridge.ts` (chat request → image generation).
//! - `form` — the `FormData` semantics multipart payloads rely on.
pub mod bridge;
pub mod dispatcher;
pub mod form;
pub mod image;
pub mod transformers;

pub use dispatcher::{MediaHost, MediaResult};
