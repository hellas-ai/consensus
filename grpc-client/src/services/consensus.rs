//! Consensus service implementation for L-notarization queries.

use ark_serialize::CanonicalSerialize;
use tonic::{Request, Response, Status};

use crate::proto::consensus_service_server::ConsensusService;
use crate::proto::{
    Empty, ErrorCode, GetLNotarizationByHeightRequest, GetLNotarizationRequest,
    LNotarizationResponse, ValidatorSetResponse,
};
use crate::server::ReadOnlyContext;

use super::utils::parse_hash;

/// Implementation of the ConsensusService gRPC service.
///
/// Provides access to L-notarization certificates (finality proofs) for light client
/// verification and validator set information.
pub struct ConsensusServiceImpl {
    context: ReadOnlyContext,
}

impl ConsensusServiceImpl {
    /// Create a new ConsensusService implementation.
    pub fn new(context: ReadOnlyContext) -> Self {
        Self { context }
    }
}

/// Network consensus parameters - must match the actual node configuration
const N: usize = 6;
const F: usize = 1;

#[tonic::async_trait]
impl ConsensusService for ConsensusServiceImpl {
    /// Get L-notarization by block hash.
    async fn get_l_notarization(
        &self,
        request: Request<GetLNotarizationRequest>,
    ) -> Result<Response<LNotarizationResponse>, Status> {
        let req = request.into_inner();
        let block_hash = parse_hash(&req.block_hash)?;

        let l_notarization = self
            .context
            .store
            .get_l_notarization::<N, F>(&block_hash)
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        match l_notarization {
            Some(l) => {
                // Serialize aggregated signature to hex
                let mut sig_bytes = Vec::new();
                l.aggregated_signature
                    .serialize_compressed(&mut sig_bytes)
                    .map_err(|e| {
                        Status::internal(format!("Signature serialization error: {}", e))
                    })?;

                Ok(Response::new(LNotarizationResponse {
                    view: l.view,
                    block_hash: hex::encode(l.block_hash),
                    height: l.height,
                    aggregated_signature: hex::encode(sig_bytes),
                    peer_ids: l.peer_ids.to_vec(),
                    error: ErrorCode::Unspecified as i32,
                }))
            }
            None => Ok(Response::new(LNotarizationResponse {
                view: 0,
                block_hash: String::new(),
                height: 0,
                aggregated_signature: String::new(),
                peer_ids: vec![],
                error: ErrorCode::NotFound as i32,
            })),
        }
    }

    /// Get L-notarization by block height.
    async fn get_l_notarization_by_height(
        &self,
        request: Request<GetLNotarizationByHeightRequest>,
    ) -> Result<Response<LNotarizationResponse>, Status> {
        let req = request.into_inner();

        let l_notarization = self
            .context
            .store
            .get_l_notarization_by_height::<N, F>(req.height)
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        match l_notarization {
            Some(l) => {
                // Serialize aggregated signature to hex
                let mut sig_bytes = Vec::new();
                l.aggregated_signature
                    .serialize_compressed(&mut sig_bytes)
                    .map_err(|e| {
                        Status::internal(format!("Signature serialization error: {}", e))
                    })?;

                Ok(Response::new(LNotarizationResponse {
                    view: l.view,
                    block_hash: hex::encode(l.block_hash),
                    height: l.height,
                    aggregated_signature: hex::encode(sig_bytes),
                    peer_ids: l.peer_ids.to_vec(),
                    error: ErrorCode::Unspecified as i32,
                }))
            }
            None => Ok(Response::new(LNotarizationResponse {
                view: 0,
                block_hash: String::new(),
                height: 0,
                aggregated_signature: String::new(),
                peer_ids: vec![],
                error: ErrorCode::NotFound as i32,
            })),
        }
    }

    /// Get the current validator set.
    async fn get_validator_set(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ValidatorSetResponse>, Status> {
        // TODO: Access the actual PeerSet from node configuration
        // For now, return placeholder data indicating we need runtime configuration
        // The actual validator set should come from the node's P2P layer or genesis config

        // Placeholder response - in production, this would query the actual validator set
        Ok(Response::new(ValidatorSetResponse {
            validators: vec![], // Would be populated from PeerSet
            total_validators: N as u32,
            fault_tolerance: F as u32,
        }))
    }
}
