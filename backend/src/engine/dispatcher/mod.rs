//! The dispatch loop and core inference routes on the engine:
//! `services/dispatch/{dispatcher,request-manager,standard-attempt-request,
//! transformer-factory}.ts`, `routes/inference/{chat,messages,gemini,
//! responses,completions}.ts`, plus the Trefoil glue (`engine`) the routing
//! and observability ports run on.
pub mod completions;
pub mod cooldown_parsers;
pub mod debug;
pub mod engine;
pub mod oauth;
pub mod ip;
pub mod limits;
pub mod ollama;
pub mod quota;
pub mod reasoning_log;
pub mod request_manager;
pub mod responses_storage;
pub mod routes;
pub mod stall;
pub mod stall_inspector;
pub mod transformer_factory;
pub mod vision;
