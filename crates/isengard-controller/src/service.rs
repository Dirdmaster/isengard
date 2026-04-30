//! Stub `Controller` gRPC service. Both RPCs return [`Status::unimplemented`].
//! Phase 2c–2e replace these with real handlers.

use std::sync::Arc;

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse};
use isengard_storage::Inventory;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Clone)]
pub struct ControllerService {
    // Task 4 of Phase 2c consumes this from inside the `enroll` handler.
    #[allow(dead_code)]
    inventory: Arc<Inventory>,
}

impl ControllerService {
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    type SyncStream = ReceiverStream<Result<ControllerMessage, Status>>;

    async fn enroll(
        &self,
        _request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        tracing::debug!("Enroll RPC reached stub — returning Unimplemented");
        Err(Status::unimplemented("Enroll handler lands in Phase 2c"))
    }

    async fn sync(
        &self,
        _request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        tracing::debug!("Sync RPC reached stub — returning Unimplemented");
        Err(Status::unimplemented("Sync handler lands in Phase 2e"))
    }
}
