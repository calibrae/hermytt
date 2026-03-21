pub mod mqtt;
pub mod rest;
pub mod tcp;
pub mod telegram;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use hermytt_core::SessionManager;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Start the transport, serving the given session manager.
    async fn serve(self: Arc<Self>, sessions: Arc<SessionManager>) -> Result<()>;

    /// Transport name for logging.
    fn name(&self) -> &str;
}
