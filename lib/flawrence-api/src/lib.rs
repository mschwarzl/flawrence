pub mod anthropic;
pub mod batch;
pub mod chat_client;
pub mod pricing;
pub mod retry;
pub mod routing;
pub mod streaming;
pub mod strict;

pub use anthropic::AnthropicClient;
pub use batch::BatchClient;
pub use chat_client::ChatClient;
pub use routing::RoutingClient;
pub use streaming::{OpenAiAuth, StreamingClient};

/// User-Agent string sent on every outbound HTTP request.
pub const USER_AGENT: &str = "Flawrence/0.1";
