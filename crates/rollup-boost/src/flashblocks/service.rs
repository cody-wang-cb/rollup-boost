use super::outbound::WebSocketPublisher;
use super::primitives::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use crate::RpcClientError;
use crate::{
    ClientResult, EngineApiExt, NewPayload, OpExecutionPayloadEnvelope, PayloadVersion, RpcClient,
};
use alloy_primitives::U256;
use alloy_rpc_types_engine::{
    BlobsBundleV1, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
};
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus};
use alloy_rpc_types_eth::{Block, BlockNumberOrTag};
use core::net::SocketAddr;
use jsonrpsee::core::async_trait;
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tracing::error;

#[derive(Debug, Error)]
pub enum FlashblocksError {
    #[error("Missing base payload for initial flashblock")]
    MissingBasePayload,
    #[error("Unexpected base payload for non-initial flashblock")]
    UnexpectedBasePayload,
    #[error("Missing delta for flashblock")]
    MissingDelta,
    #[error("Invalid index for flashblock")]
    InvalidIndex,
    #[error("Missing payload")]
    MissingPayload,
}

impl From<FlashblocksError> for RpcClientError {
    fn from(err: FlashblocksError) -> Self {
        RpcClientError::InvalidPayload(err.to_string())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct FlashbotsMessage {
    method: String,
    params: serde_json::Value,
    #[serde(default)]
    id: Option<u64>,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum FlashblocksEngineMessage {
    FlashblocksPayloadV1(FlashblocksPayloadV1),
}

#[derive(Debug, Default)]
struct FlashblockBuilder {
    base: Option<ExecutionPayloadBaseV1>,
    flashblocks: Vec<ExecutionPayloadFlashblockDeltaV1>,
}

impl FlashblockBuilder {
    pub fn new() -> Self {
        Self {
            base: None,
            flashblocks: Vec::new(),
        }
    }

    pub fn extend(&mut self, payload: FlashblocksPayloadV1) -> Result<(), FlashblocksError> {
        tracing::debug!(message = "Extending payload", payload_id = %payload.payload_id, index = payload.index, has_base=payload.base.is_some());

        // Check base payload rules
        match (payload.index, payload.base) {
            // First payload must have a base
            (0, None) => return Err(FlashblocksError::MissingBasePayload),
            (0, Some(base)) => self.base = Some(base),
            // Subsequent payloads must have no base
            (_, Some(_)) => return Err(FlashblocksError::UnexpectedBasePayload),
            (_, None) => {} // Non-zero index without base is fine
        }

        // Validate the index is contiguous
        if payload.index != self.flashblocks.len() as u64 {
            return Err(FlashblocksError::InvalidIndex);
        }

        // Update latest diff and accumulate transactions and withdrawals
        self.flashblocks.push(payload.diff);

        Ok(())
    }

    pub fn into_envelope(
        self,
        version: PayloadVersion,
    ) -> Result<OpExecutionPayloadEnvelope, FlashblocksError> {
        let base = self.base.ok_or(FlashblocksError::MissingPayload)?;

        // There must be at least one delta
        let diff = self
            .flashblocks
            .last()
            .ok_or(FlashblocksError::MissingDelta)?;

        let transactions = self
            .flashblocks
            .iter()
            .flat_map(|diff| diff.transactions.clone())
            .collect();

        let withdrawals = self
            .flashblocks
            .iter()
            .flat_map(|diff| diff.withdrawals.clone())
            .collect();

        let withdrawals_root = diff.withdrawals_root;

        let execution_payload = ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                withdrawals,
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: base.parent_hash,
                    fee_recipient: base.fee_recipient,
                    state_root: diff.state_root,
                    receipts_root: diff.receipts_root,
                    logs_bloom: diff.logs_bloom,
                    prev_randao: base.prev_randao,
                    block_number: base.block_number,
                    gas_limit: base.gas_limit,
                    gas_used: diff.gas_used,
                    timestamp: base.timestamp,
                    extra_data: base.extra_data,
                    base_fee_per_gas: base.base_fee_per_gas,
                    block_hash: diff.block_hash,
                    transactions,
                },
            },
        };

        match version {
            PayloadVersion::V3 => Ok(OpExecutionPayloadEnvelope::V3(
                OpExecutionPayloadEnvelopeV3 {
                    parent_beacon_block_root: base.parent_beacon_block_root,
                    block_value: U256::ZERO,
                    blobs_bundle: BlobsBundleV1::default(),
                    should_override_builder: false,
                    execution_payload,
                },
            )),
            PayloadVersion::V4 => Ok(OpExecutionPayloadEnvelope::V4(
                OpExecutionPayloadEnvelopeV4 {
                    parent_beacon_block_root: base.parent_beacon_block_root,
                    block_value: U256::ZERO,
                    blobs_bundle: BlobsBundleV1::default(),
                    should_override_builder: false,
                    execution_payload: OpExecutionPayloadV4 {
                        withdrawals_root,
                        payload_inner: execution_payload,
                    },
                    execution_requests: vec![],
                },
            )),
        }
    }
}

#[derive(Clone)]
pub struct FlashblocksService {
    client: RpcClient,

    // Current payload ID we're processing (set from external notification)
    current_payload_id: Arc<RwLock<PayloadId>>,

    // flashblocks payload being constructed
    best_payload: Arc<RwLock<FlashblockBuilder>>,

    // websocket publisher for sending valid preconfirmations to clients
    ws_pub: Arc<WebSocketPublisher>,
}

impl FlashblocksService {
    pub fn new(client: RpcClient, outbound_addr: SocketAddr) -> eyre::Result<Self> {
        let ws_pub = WebSocketPublisher::new(outbound_addr)?.into();

        Ok(Self {
            client,
            current_payload_id: Arc::new(RwLock::new(PayloadId::default())),
            best_payload: Arc::new(RwLock::new(FlashblockBuilder::new())),
            ws_pub,
        })
    }

    pub async fn get_best_payload(
        &self,
        version: PayloadVersion,
    ) -> Result<Option<OpExecutionPayloadEnvelope>, FlashblocksError> {
        // consume the best payload and reset the builder
        let payload = {
            let mut builder = self.best_payload.write().await;
            std::mem::take(&mut *builder).into_envelope(version)?
        };
        *self.best_payload.write().await = FlashblockBuilder::new();

        Ok(Some(payload))
    }

    pub async fn set_current_payload_id(&self, payload_id: PayloadId) {
        tracing::debug!(message = "Setting current payload ID", payload_id = %payload_id);
        *self.current_payload_id.write().await = payload_id;
    }

    async fn on_event(&mut self, event: FlashblocksEngineMessage) {
        match event {
            FlashblocksEngineMessage::FlashblocksPayloadV1(payload) => {
                tracing::debug!(
                    message = "Received flashblock payload",
                    payload_id = %payload.payload_id,
                    index = payload.index
                );

                // make sure the payload id matches the current payload id
                if *self.current_payload_id.read().await != payload.payload_id {
                    error!(message = "Payload ID mismatch",);
                    return;
                }

                if let Err(e) = self.best_payload.write().await.extend(payload.clone()) {
                    error!(message = "Failed to extend payload", error = %e);
                } else {
                    // Broadcast the valid message
                    if let Err(e) = self.ws_pub.publish(&payload) {
                        error!(message = "Failed to broadcast payload", error = %e);
                    }
                }
            }
        }
    }

    pub async fn run(&mut self, mut stream: mpsc::Receiver<FlashblocksPayloadV1>) {
        while let Some(event) = stream.recv().await {
            self.on_event(FlashblocksEngineMessage::FlashblocksPayloadV1(event))
                .await;
        }
    }
}

#[async_trait]
impl EngineApiExt for FlashblocksService {
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> ClientResult<ForkchoiceUpdated> {
        let result = self
            .client
            .fork_choice_updated_v3(fork_choice_state, payload_attributes)
            .await?;

        if let Some(payload_id) = result.payload_id {
            tracing::debug!(message = "Forkchoice updated", payload_id = %payload_id);
            self.set_current_payload_id(payload_id).await;
        } else {
            tracing::debug!(message = "Forkchoice updated with no payload ID");
        }
        Ok(result)
    }

    async fn new_payload(&self, new_payload: NewPayload) -> ClientResult<PayloadStatus> {
        self.client.new_payload(new_payload).await
    }

    async fn get_payload(
        &self,
        payload_id: PayloadId,
        version: PayloadVersion,
    ) -> ClientResult<OpExecutionPayloadEnvelope> {
        let fb_payload = self.get_best_payload(version).await?;
        if let Some(payload) = fb_payload {
            tracing::info!(message = "Returning fb payload", payload_id = %payload_id);
            return Ok(payload);
        }

        tracing::info!(message = "No flashblocks payload available, fetching from client", payload_id = %payload_id);
        let result = self.client.get_payload(payload_id, version).await?;
        Ok(result)
    }

    async fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> ClientResult<Block> {
        self.client.get_block_by_number(number, full).await
    }
}
