use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::Server;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_DEBUG_API_PORT: u16 = 5555;

#[derive(Serialize, Deserialize, Debug)]
pub enum SetDryRunRequestAction {
    ToggleDryRun,
    SetDryRun(bool),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetDryRunRequest {
    pub action: SetDryRunRequestAction,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SetDryRunResponse {
    pub dry_run_state: bool,
}

#[rpc(server, client, namespace = "debug")]
trait DebugApi {
    #[method(name = "setDryRun")]
    async fn set_dry_run(&self, request: SetDryRunRequest) -> RpcResult<SetDryRunResponse>;
}

pub struct DebugServer {
    dry_run: Arc<Mutex<bool>>,
}

impl DebugServer {
    pub fn new(dry_run: Arc<Mutex<bool>>) -> Self {
        Self { dry_run }
    }

    pub async fn run(self, port: Option<u16>) -> eyre::Result<()> {
        let port = port.unwrap_or(DEFAULT_DEBUG_API_PORT);

        let server = Server::builder()
            .build(format!("127.0.0.1:{}", port))
            .await?;

        let handle = server.start(self.into_rpc());

        tracing::info!("Debug server started on port {}", port);

        // In this example we don't care about doing shutdown so let's it run forever.
        // You may use the `ServerHandle` to shut it down or manage it yourself.
        tokio::spawn(handle.stopped());

        Ok(())
    }
}

#[async_trait]
impl DebugApiServer for DebugServer {
    async fn set_dry_run(&self, _request: SetDryRunRequest) -> RpcResult<SetDryRunResponse> {
        let mut dry_run = self.dry_run.lock().await;

        match _request.action {
            SetDryRunRequestAction::ToggleDryRun => {
                *dry_run = !*dry_run;
            }
            SetDryRunRequestAction::SetDryRun(state) => {
                *dry_run = state;
            }
        };

        Ok(SetDryRunResponse {
            dry_run_state: *dry_run,
        })
    }
}

pub struct DebugClient {
    client: HttpClient,
}

impl DebugClient {
    pub fn new(url: &str) -> eyre::Result<Self> {
        let client = HttpClient::builder().build(url)?;

        Ok(Self { client })
    }

    pub async fn set_dry_run(
        &self,
        action: SetDryRunRequestAction,
    ) -> eyre::Result<SetDryRunResponse> {
        let request = SetDryRunRequest { action };
        let result = DebugApiClient::set_dry_run(&self.client, request).await?;
        Ok(result)
    }
}

impl Default for DebugClient {
    fn default() -> Self {
        Self::new(format!("http://localhost:{}", DEFAULT_DEBUG_API_PORT).as_str()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_debug_client() {
        // spawn the server and try to modify it with the client
        let dry_run = Arc::new(Mutex::new(false));

        let server = DebugServer::new(dry_run.clone());
        let _ = server.run(None).await.unwrap();

        let client = DebugClient::default();
        let result = client
            .set_dry_run(SetDryRunRequestAction::ToggleDryRun)
            .await
            .unwrap();

        assert_eq!(result.dry_run_state, true);
        assert_eq!(result.dry_run_state, *dry_run.lock().await);

        let result = client
            .set_dry_run(SetDryRunRequestAction::ToggleDryRun)
            .await
            .unwrap();
        assert_eq!(result.dry_run_state, false);
        assert_eq!(result.dry_run_state, *dry_run.lock().await);
    }
}
