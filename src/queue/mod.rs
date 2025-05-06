use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use crate::webhook::types::JobData;
use crate::error::Result;
use crate::webhook::sender::JobProcessorFnAsync;

pub mod manager;
pub mod redis_queue_manager;
pub mod memory_queue_manager;
pub mod sqs_queue_manager;

impl JobData where JobData: Serialize + DeserializeOwned {}

// Define a type alias for the callback for clarity and easier management
type JobProcessorFn = Box<dyn Fn(JobData) -> Result<()> + Send + Sync + 'static>;
// Define a type alias for the Arc'd callback used in Redis manager
type ArcJobProcessorFn = Arc<Box<
    dyn Fn(JobData) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync + 'static,
>>;

#[async_trait]
pub trait QueueInterface: Send + Sync {
    async fn add_to_queue(&self, queue_name: &str, data: JobData) -> crate::error::Result<()>;
    // Changed callback type to accept 'static lifetime needed by Redis workers
    async fn process_queue(&self, queue_name: &str, callback: JobProcessorFnAsync) -> crate::error::Result<()>;
    async fn disconnect(&self) -> crate::error::Result<()>;
}
