//! Serving the index to other machines: the JSON exchanged, the access token
//! and a transport-free request handler, the HTTP server around it, and the
//! client that asks another machine's server.

pub mod client;
pub mod handler;
pub mod server;
pub mod token;
pub mod wire;

/// The wire protocol version carried in every response.
pub const PROTOCOL: u32 = 1;
