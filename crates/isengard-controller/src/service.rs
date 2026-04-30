//! `Controller` gRPC service. `enroll` is real (Phase 2c); `sync` is still a stub.

use std::sync::Arc;

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse};
use isengard_storage::Inventory;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Clone)]
pub struct ControllerService {
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
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let req = request.into_inner();

        // Convert proto request → storage request.
        let enroll_req = isengard_storage::EnrollHost {
            fingerprint: req.fingerprint.clone(),
            hostname: req.hostname.clone(),
            os: req.os,
            arch: req.arch,
            agent_version: req.agent_version,
            docker_version: req.docker_version,
        };

        let id = match self.inventory.enroll_host(enroll_req).await {
            Ok(id) => id,
            Err(isengard_storage::Error::Db(sqlx_err)) => {
                // UNIQUE violation on fingerprint maps to AlreadyExists.
                let is_unique_violation = sqlx_err
                    .as_database_error()
                    .map(|db_err| {
                        db_err.message().contains("UNIQUE")
                            || db_err.code().as_deref() == Some("2067")
                            || db_err.code().as_deref() == Some("19")
                    })
                    .unwrap_or(false);
                if is_unique_violation {
                    return Err(Status::already_exists(format!(
                        "host with fingerprint {:?} is already enrolled",
                        req.fingerprint,
                    )));
                }
                tracing::error!(error = %sqlx_err, "Enroll: db error");
                return Err(Status::internal("database error"));
            }
            Err(other) => {
                tracing::error!(error = %other, "Enroll: unexpected error");
                return Err(Status::internal("storage error"));
            }
        };

        let server_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Ok(Response::new(EnrollResponse {
            agent_id: id.to_string(),
            heartbeat_interval_secs: 10,
            server_time_ms,
        }))
    }

    async fn sync(
        &self,
        _request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        tracing::debug!("Sync RPC reached stub — returning Unimplemented");
        Err(Status::unimplemented("Sync handler lands in Phase 2e"))
    }
}
