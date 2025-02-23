use super::*;
use crate::system::System;
use ces_crypto::{key_share, sr25519::KDF, SecretKey};
use ces_types::{
    attestation::{validate as validate_attestation_report, IasFields},
    messaging::EncryptedKey,
    wrap_content_to_sign, AttestationReport, ChallengeHandlerInfo, EncryptedWorkerKey, HandoverChallenge,
    MasterKeyApplyPayload, SignedContentType, WorkerEndpointPayload, WorkerRegistrationInfo,
};
use cestory_api::{
    blocks::{self, StorageState},
    crpc::{self as pb, ceseal_api_server::CesealApi},
};
use parity_scale_codec::Error as ScaleDecodeError;
use std::{borrow::Borrow, fmt::Debug, time::Duration};
use thiserror::Error;
use tonic::{Request, Response, Status};
use tracing::{error, info};

type RpcResult<T> = anyhow::Result<Response<T>, Status>;

pub struct RpcService<Platform> {
    pub(crate) cestory: CesealSafeBox<Platform>,
}

impl<Platform: pal::Platform> RpcService<Platform> {
    pub fn new_with(cestory: CesealSafeBox<Platform>) -> RpcService<Platform> {
        RpcService { cestory }
    }

    pub fn new(platform: Platform) -> RpcService<Platform> {
        RpcService { cestory: CesealSafeBox::new(platform, None) }
    }
}

#[derive(Error, Debug)]
pub enum CesealServiceError {
    #[error(transparent)]
    CesealLock(#[from] CesealLockError),

    /// Failed to decode the request parameters
    #[error("{0}")]
    DecodeError(String),

    /// Some error occurred when handling the request
    #[error("{0}")]
    AppError(String),

    #[error(transparent)]
    Anyhow(anyhow::Error),
}

impl From<ScaleDecodeError> for CesealServiceError {
    fn from(e: ScaleDecodeError) -> Self {
        Self::DecodeError(e.to_string())
    }
}

impl From<serde_json::Error> for CesealServiceError {
    fn from(e: serde_json::Error) -> Self {
        Self::DecodeError(e.to_string())
    }
}

impl From<CesealServiceError> for Status {
    fn from(value: CesealServiceError) -> Self {
        Status::internal(value.to_string())
    }
}

impl From<CesealLockError> for Status {
    fn from(value: CesealLockError) -> Self {
        Status::internal(value.to_string())
    }
}

fn to_status(err: ScaleDecodeError) -> Status {
    Status::internal(err.to_string())
}

fn from_display(e: impl core::fmt::Display) -> CesealServiceError {
    CesealServiceError::AppError(e.to_string())
}

fn from_debug(e: impl core::fmt::Debug) -> CesealServiceError {
    CesealServiceError::AppError(format!("{e:?}"))
}

impl From<types::Error> for Status {
    fn from(value: types::Error) -> Self {
        Status::internal(value.to_string())
    }
}

fn now() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
    now.as_secs()
}

type CesealResult<T> = anyhow::Result<T, CesealServiceError>;

impl<Platform: pal::Platform> RpcService<Platform> {
    pub fn lock_ceseal(
        &self,
        allow_rcu: bool,
        allow_safemode: bool,
    ) -> Result<LogOnDrop<MutexGuard<'_, Ceseal<Platform>>>, CesealLockError> {
        self.cestory.lock(allow_rcu, allow_safemode)
    }
}

fn create_attestation_report_on<Platform: pal::Platform>(
    platform: &Platform,
    attestation_provider: Option<AttestationProvider>,
    data: &[u8],
    timeout: Duration,
    max_retries: u32,
) -> CesealResult<pb::Attestation> {
    let mut tried = 0;
    let encoded_report = loop {
        break match platform.create_attestation_report(attestation_provider, data, timeout) {
            Ok(r) => r,
            Err(e) => {
                let message = format!("Failed to create attestation report: {e:?}");
                error!("{}", message);
                if tried >= max_retries {
                    return Err(from_display(message))
                }
                let sleep_secs = (1 << tried).min(8);
                info!("Retrying after {} seconds...", sleep_secs);
                std::thread::sleep(Duration::from_secs(sleep_secs));
                tried += 1;
                continue
            },
        }
    };
    Ok(pb::Attestation {
        version: 1,
        provider: serde_json::to_string(&attestation_provider).unwrap(),
        payload: None,
        encoded_report,
        timestamp: now(),
    })
}

#[tonic::async_trait]
impl<Platform: pal::Platform + Serialize + DeserializeOwned> CesealApi for RpcService<Platform> {
    /// Get basic information about Ceseal state.
    async fn get_info(&self, _request: Request<()>) -> RpcResult<pb::CesealInfo> {
        let info = self.lock_ceseal(true, true)?.get_info();
        #[cfg(target_env = "gnu")]
        info!("Got info: {:?} mallinfo: {:?}", info.debug_info(), unsafe { libc::mallinfo() });
        #[cfg(not(target_env = "gnu"))]
        info!("Got info: {:?}", info.debug_info());
        Ok(Response::new(info))
    }

    /// Sync the parent chain header
    async fn sync_header(&self, request: Request<pb::HeadersToSync>) -> RpcResult<pb::SyncedTo> {
        let request = request.into_inner();
        let headers = request.decode_headers().map_err(to_status)?;
        let authority_set_change = request.decode_authority_set_change().map_err(to_status)?;
        let result = self.lock_ceseal(false, true)?.sync_header(headers, authority_set_change)?;
        Ok(Response::new(result))
    }

    /// Dispatch blocks (Sync storage changes)
    async fn dispatch_blocks(&self, request: Request<pb::Blocks>) -> RpcResult<pb::SyncedTo> {
        let request = request.into_inner();
        let blocks = request.decode_blocks().map_err(to_status)?;
        //FIXME: The RCU lock policy maybe not suitable for ceseal,
        // because the chain storage state in ceseal need to share with other service readonly, we don't need a mutex
        // unnecessary. But adding a long-period lock to the block dispatch process (which can take a long time)
        // is a bad idea. So there may be a solution:
        // 1. Use RwLock for the CESEAL instance;
        // 2. Or refactor ceseal to reduce the granularity of CESEAL locks.
        // However, now in order to avoid cloning the ceseal instance (as we do not want to use mutex on its internal
        // state), we have simply locked it. Remember to optimize here!
        let synced_to = self.lock_ceseal(false, true)?.dispatch_blocks(blocks);
        info!("Blocks are dispatched");
        Ok(Response::new(synced_to?))
    }

    /// Init the Ceseal runtime
    async fn init_runtime(&self, request: Request<pb::InitRuntimeRequest>) -> RpcResult<pb::InitRuntimeResponse> {
        let request = request.into_inner();
        let result = self.lock_ceseal(false, false)?.init_runtime(
            request.decode_genesis_info().map_err(to_status)?,
            request.decode_genesis_state().map_err(to_status)?,
            request.decode_operator().map_err(to_status)?,
            request.debug_set_key.clone(),
            request.decode_attestation_provider().map_err(to_status)?,
        )?;
        Ok(Response::new(result))
    }

    /// Get the cached Ceseal runtime init response
    async fn get_runtime_info(
        &self,
        request: Request<pb::GetRuntimeInfoRequest>,
    ) -> RpcResult<pb::InitRuntimeResponse> {
        let request = request.into_inner();
        let resp = self
            .lock_ceseal(true, false)?
            .get_runtime_info(request.force_refresh_ra, request.decode_operator().map_err(to_status)?)?;
        Ok(Response::new(resp))
    }

    /// Get pending egress messages
    async fn get_egress_messages(&self, _: Request<()>) -> RpcResult<pb::GetEgressMessagesResponse> {
        let resp = self
            .lock_ceseal(true, false)?
            .get_egress_messages()
            .map(pb::GetEgressMessagesResponse::new)?;
        Ok(Response::new(resp))
    }

    /// Init the endpoint
    async fn set_endpoint(&self, request: Request<pb::SetEndpointRequest>) -> RpcResult<pb::GetEndpointResponse> {
        let request = request.into_inner();
        let resp = self.lock_ceseal(false, false)?.set_endpoint(request.endpoint)?;
        Ok(Response::new(resp))
    }

    /// Refresh the endpoint signing time
    async fn refresh_endpoint_signing_time(&self, _: Request<()>) -> RpcResult<pb::GetEndpointResponse> {
        Ok(Response::new(self.lock_ceseal(false, false)?.sign_endpoint()?))
    }

    /// Get endpoint info
    async fn get_endpoint_info(&self, _: Request<()>) -> RpcResult<pb::GetEndpointResponse> {
        Ok(Response::new(self.lock_ceseal(true, false)?.get_endpoint_info()?))
    }

    async fn get_master_key_apply(&self, _: Request<()>) -> RpcResult<pb::GetMasterKeyApplyResponse> {
        Ok(Response::new(self.lock_ceseal(true, false)?.get_master_key_apply()?))
    }

    async fn operate_external_server(&self, request: Request<pb::ExternalServerOperation>) -> RpcResult<()> {
        use pb::ExternalServerCmd::*;
        let request = request.into_inner();
        match request.cmd() {
            Start => self
                .lock_ceseal(false, false)?
                .start_external_server()
                .map_err(|e| Error::Anyhow(e))?,
            Shutdown => {
                let stub = {
                    let mut ceseal = self.lock_ceseal(false, false)?;
                    let Some(stub) = ceseal.external_server_stub.take() else {
                        return Err(Error::ExternalServerAlreadyClosed.into())
                    };
                    stub
                };
                let _ = stub.shutdown_tx.send(());
                let _ = stub.stopped_rx.await;
            },
        }
        Ok(Response::new(()))
    }

    /// A echo rpc to measure network RTT.
    async fn echo(&self, request: Request<pb::EchoMessage>) -> RpcResult<pb::EchoMessage> {
        let echo_msg = request.into_inner().echo_msg;
        Ok(Response::new(pb::EchoMessage { echo_msg }))
    }

    /// Key Handover Server: Get challenge for worker key handover from another ceSeal
    async fn handover_create_challenge(&self, _: Request<()>) -> RpcResult<pb::HandoverChallenge> {
        let mut cestory = self.lock_ceseal(false, true)?;
        let (block, ts) = cestory.current_block()?;
        let challenge = cestory.get_worker_key_challenge(block, ts);
        Ok(Response::new(pb::HandoverChallenge::new(challenge)))
    }

    /// Key Handover Server: Get worker key with RA report on challenge from another Ceseal
    async fn handover_start(
        &self,
        request: Request<pb::HandoverChallengeResponse>,
    ) -> RpcResult<pb::HandoverWorkerKey> {
        let request = request.into_inner();
        let mut cestory = self.lock_ceseal(false, true)?;
        let attestation_provider = cestory.attestation_provider;
        let dev_mode = cestory.dev_mode;
        let in_sgx = attestation_provider == Some(AttestationProvider::Ias);
        let (block_number, now_ms) = cestory.current_block()?;

        // 1. verify client RA report to ensure it's in sgx
        // this also ensure the message integrity
        let challenge_handler = request.decode_challenge_handler().map_err(from_display)?;
        let block_sec = now_ms / 1000;
        let attestation = if !dev_mode && in_sgx {
            let payload_hash = sp_core::hashing::blake2_256(&challenge_handler.encode());
            let raw_attestation = request
                .attestation
                .ok_or_else(|| from_display("Client attestation not found"))?;
            let attn_to_validate = Option::<AttestationReport>::decode(&mut &raw_attestation.encoded_report[..])
                .map_err(|_| from_display("Decode client attestation failed"))?;
            // The time from attestation report is generated by IAS, thus trusted. By default, it's valid for **10h**.
            // By ensuring our system timestamp is within the valid period, we know that this ceseal is not hold back by
            // malicious workers.
            validate_attestation_report(attn_to_validate.clone(), &payload_hash, block_sec, false, vec![], false)
                .map_err(|_| from_display("Invalid client RA report"))?;
            attn_to_validate
        } else {
            info!("Skip client RA report check in dev mode");
            None
        };
        // 2. verify challenge validity to prevent replay attack
        let challenge = challenge_handler.challenge;
        if !cestory.verify_worker_key_challenge(&challenge) {
            return Err(Status::invalid_argument("Invalid challenge"))
        }
        // 3. verify sgx local attestation report to ensure the handover ceseals are on the same machine
        if !dev_mode && in_sgx {
            let recv_local_report = unsafe {
                sgx_api_lite::decode(&challenge_handler.sgx_local_report)
                    .map_err(|_| from_display("Invalid client LA report"))?
            };
            sgx_api_lite::verify(recv_local_report).map_err(|_| from_display("No remote handover"))?;
        } else {
            info!("Skip client LA report check in dev mode");
        }
        // 4. verify challenge block height and report timestamp
        // only challenge within 150 blocks (30 minutes) is accepted
        let challenge_height = challenge.block_number;
        if !(challenge_height <= block_number && block_number - challenge_height <= 150) {
            return Err(Status::invalid_argument("Outdated challenge"))
        }
        // 5. verify ceseal launch date, never handover to old ceseal
        if !dev_mode && in_sgx {
            let my_la_report = {
                // target_info and reportdata not important, we just need the report metadata
                let target_info = sgx_api_lite::target_info().expect("should not fail in SGX; qed.");
                sgx_api_lite::report(&target_info, &[0; 64])
                    .map_err(|_| from_display("Cannot read server ceseal info"))?
            };
            let my_runtime_hash = {
                let ias_fields = IasFields {
                    mr_enclave: my_la_report.body.mr_enclave.m,
                    mr_signer: my_la_report.body.mr_signer.m,
                    isv_prod_id: my_la_report.body.isv_prod_id.to_ne_bytes(),
                    isv_svn: my_la_report.body.isv_svn.to_ne_bytes(),
                    report_data: [0; 64],
                    confidence_level: 0,
                };
                ias_fields.extend_mrenclave()
            };
            let runtime_state = cestory.runtime_state()?;
            let my_runtime_timestamp = runtime_state
                .chain_storage
                .read()
                .get_ceseal_bin_added_at(&my_runtime_hash)
                .ok_or_else(|| from_display("Server ceseal not allowed on chain"))?;

            let attestation = attestation.ok_or_else(|| from_display("Client attestation not found"))?;
            let runtime_hash = match attestation {
                AttestationReport::SgxIas { ra_report, signature: _, raw_signing_cert: _ } => {
                    let (ias_fields, _) =
                        IasFields::from_ias_report(&ra_report).map_err(|_| from_display("Invalid client RA report"))?;
                    ias_fields.extend_mrenclave()
                },
            };
            let req_runtime_timestamp = runtime_state
                .chain_storage
                .read()
                .get_ceseal_bin_added_at(&runtime_hash)
                .ok_or_else(|| from_display("Client ceseal not allowed on chain"))?;

            if my_runtime_timestamp >= req_runtime_timestamp {
                return Err(Status::internal("No handover for old ceseal"))
            }
        } else {
            info!("Skip ceseal timestamp check in dev mode");
        }

        // Share the key with attestation
        let ecdh_pubkey = challenge_handler.ecdh_pubkey;
        let iv = crate::generate_random_iv();
        let runtime_data = cestory.persistent_runtime_data().map_err(from_display)?;
        let (my_identity_key, _) = runtime_data.decode_keys();
        let (ecdh_pubkey, encrypted_key) = key_share::encrypt_secret_to(
            &my_identity_key,
            &[b"worker_key_handover"],
            &ecdh_pubkey.0,
            &SecretKey::Sr25519(runtime_data.sk),
            &iv,
        )
        .map_err(from_debug)?;
        let encrypted_key = EncryptedKey { ecdh_pubkey: sr25519::Public(ecdh_pubkey), encrypted_key, iv };
        let runtime_state = cestory.runtime_state()?;
        let genesis_block_hash = runtime_state.genesis_block_hash;
        let encrypted_worker_key = EncryptedWorkerKey { genesis_block_hash, dev_mode, encrypted_key };

        let worker_key_hash = sp_core::hashing::blake2_256(&encrypted_worker_key.encode());
        let attestation = if !dev_mode && in_sgx {
            Some(create_attestation_report_on(
                &cestory.platform,
                attestation_provider,
                &worker_key_hash,
                cestory.args.ra_timeout,
                cestory.args.ra_max_retries,
            )?)
        } else {
            info!("Omit RA report in workerkey response in dev mode");
            None
        };

        Ok(Response::new(pb::HandoverWorkerKey::new(encrypted_worker_key, attestation)))
    }

    /// Key Handover Client: Process HandoverChallenge and return RA report
    async fn handover_accept_challenge(
        &self,
        request: Request<pb::HandoverChallenge>,
    ) -> RpcResult<pb::HandoverChallengeResponse> {
        let mut cestory = self.lock_ceseal(false, true)?;

        // generate and save tmp key only for key handover encryption
        let handover_key = crate::new_sr25519_key();
        let handover_ecdh_key = handover_key.derive_ecdh_key().expect("should never fail with valid key; qed.");
        let ecdh_pubkey = ces_types::EcdhPublicKey(handover_ecdh_key.public());
        cestory.handover_ecdh_key = Some(handover_ecdh_key);

        let request = request.into_inner();
        let challenge = request.decode_challenge().map_err(from_display)?;
        let dev_mode = challenge.dev_mode;
        // generate local attestation report to ensure the handover ceseals are on the same machine
        let sgx_local_report = if !dev_mode {
            let its_target_info = unsafe {
                sgx_api_lite::decode(&challenge.sgx_target_info)
                    .map_err(|_| from_display("Invalid client sgx target info"))?
            };
            // the report data does not matter since we only care about the origin
            let report = sgx_api_lite::report(its_target_info, &[0; 64])
                .map_err(|_| from_display("Failed to create client LA report"))?;
            sgx_api_lite::encode(&report).to_vec()
        } else {
            info!("Omit client LA report for dev mode challenge");
            vec![]
        };

        let challenge_handler = ChallengeHandlerInfo { challenge, sgx_local_report, ecdh_pubkey };
        let handler_hash = sp_core::hashing::blake2_256(&challenge_handler.encode());
        let attestation = if !dev_mode {
            Some(create_attestation_report_on(
                &cestory.platform,
                Some(AttestationProvider::Ias),
                &handler_hash,
                cestory.args.ra_timeout,
                cestory.args.ra_max_retries,
            )?)
        } else {
            info!("Omit client RA report for dev mode challenge");
            None
        };

        Ok(Response::new(pb::HandoverChallengeResponse::new(challenge_handler, attestation)))
    }

    /// Key Handover Client: Receive encrypted worker key
    async fn handover_receive(&self, request: Request<pb::HandoverWorkerKey>) -> RpcResult<()> {
        let mut cestory = self.lock_ceseal(false, true)?;
        let request = request.into_inner();
        let encrypted_worker_key = request.decode_worker_key().map_err(to_status)?;

        let dev_mode = encrypted_worker_key.dev_mode;
        // verify RA report
        if !dev_mode {
            let worker_key_hash = sp_core::hashing::blake2_256(&encrypted_worker_key.encode());
            let raw_attestation = request
                .attestation
                .ok_or_else(|| from_display("Server attestation not found"))?;
            let attn_to_validate = Option::<AttestationReport>::decode(&mut &raw_attestation.encoded_report[..])
                .map_err(|_| from_display("Decode server attestation failed"))?;
            validate_attestation_report(attn_to_validate, &worker_key_hash, now(), false, vec![], false)
                .map_err(|_| from_display("Invalid server RA report"))?;
        } else {
            info!("Skip server RA report check for dev mode key");
        }

        let encrypted_key = encrypted_worker_key.encrypted_key;
        let my_ecdh_key = cestory
            .handover_ecdh_key
            .as_ref()
            .ok_or_else(|| from_display("Handover ecdhkey not initialized"))?;
        let secret = key_share::decrypt_secret_from(
            my_ecdh_key,
            &encrypted_key.ecdh_pubkey.0,
            &encrypted_key.encrypted_key,
            &encrypted_key.iv,
        )
        .map_err(from_debug)?;

        // only seal if the key is successfully updated
        cestory
            .save_runtime_data(
                encrypted_worker_key.genesis_block_hash,
                sr25519::Pair::restore_from_secret_key(&match secret {
                    SecretKey::Sr25519(key) => key,
                    _ => panic!("Expected sr25519 key, but got rsa key."),
                }),
                false, // we are not sure whether this key is injected
                dev_mode,
            )
            .map_err(from_display)?;

        // clear cached RA report and handover ecdh key to prevent replay
        cestory.runtime_info = None;
        cestory.handover_ecdh_key = None;
        Ok(Response::new(()))
    }

    /// Load given chain state into the ceseal
    async fn load_chain_state(&self, request: Request<pb::ChainState>) -> RpcResult<()> {
        let request = request.into_inner();
        self.lock_ceseal(false, false)?
            .load_chain_state(request.block_number, request.decode_state().map_err(to_status)?)
            .map_err(from_display)?;
        Ok(Response::new(()))
    }

    /// Stop and optionally remove checkpoints
    async fn stop(&self, request: Request<pb::StopOptions>) -> RpcResult<()> {
        let request = request.into_inner();
        Ok(Response::new(self.lock_ceseal(true, true)?.stop(request.remove_checkpoints)?))
    }

    /// Partially load values into the ceseal's chain storage.
    async fn load_storage_proof(&self, request: Request<pb::StorageProof>) -> RpcResult<()> {
        let request = request.into_inner();
        self.lock_ceseal(false, true)?.load_storage_proof(request.proof)?;
        Ok(Response::new(()))
    }

    /// Take checkpoint. Returns the current block number of the saved state.
    async fn take_checkpoint(&self, _: Request<()>) -> RpcResult<pb::SyncedTo> {
        let synced_to = self.lock_ceseal(false, false)?.take_checkpoint().map_err(from_debug)?;
        Ok(Response::new(pb::SyncedTo { synced_to }))
    }
}

impl<Platform: pal::Platform + Serialize + DeserializeOwned> Ceseal<Platform> {
    fn runtime_state(&mut self) -> CesealResult<&mut RuntimeState> {
        self.runtime_state
            .as_mut()
            .ok_or_else(|| from_display("Runtime not initialized"))
    }

    fn system(&mut self) -> CesealResult<&mut System<Platform>> {
        self.system.as_mut().ok_or_else(|| from_display("Runtime not initialized"))
    }

    pub(crate) fn current_block(&mut self) -> CesealResult<(BlockNumber, u64)> {
        let now_ms = self.runtime_state()?.chain_storage.read().timestamp_now();
        let block = self
            .runtime_state()?
            .storage_synchronizer
            .counters()
            .next_block_number
            .saturating_sub(1);
        Ok((block, now_ms))
    }

    pub fn get_info(&self) -> pb::CesealInfo {
        let initialized = self.runtime_state.is_some();
        let state = self.runtime_state.as_ref();
        let genesis_block_hash = state.map(|state| hex::encode(state.genesis_block_hash));
        let dev_mode = self.dev_mode;

        let (state_root, pending_messages, counters) = match state.as_ref() {
            Some(state) => {
                let state_root = hex::encode(state.chain_storage.read().root());
                let pending_messages = state.send_mq.count_messages();
                let counters = state.storage_synchronizer.counters();
                (state_root, pending_messages, counters)
            },
            None => Default::default(),
        };

        let system_info = self.system.as_ref().map(|s| s.get_info());
        let current_block_time = match self.args.safe_mode_level {
            0 => self.system.as_ref().map(|sys| sys.now_ms).unwrap_or_default(),
            1 => self
                .runtime_state
                .as_ref()
                .map(|state| state.chain_storage.read().timestamp_now())
                .unwrap_or_default(),
            _ => 0,
        };

        let external_server_state = if self.external_server_stub.is_some() {
            ExternalServerState::Serving
        } else {
            ExternalServerState::Closed
        }
        .into();
        pb::CesealInfo {
            initialized,
            genesis_block_hash,
            headernum: counters.next_header_number,
            blocknum: counters.next_block_number,
            state_root,
            dev_mode,
            pending_messages: pending_messages as _,
            version: self.args.version.clone(),
            git_revision: self.args.git_revision.clone(),
            memory_usage: Some(self.platform.memory_usage()),
            system: system_info,
            can_load_chain_state: self.can_load_chain_state,
            safe_mode_level: self.args.safe_mode_level as _,
            current_block_time,
            external_server_state,
        }
    }

    pub(crate) fn sync_header(
        &mut self,
        headers: Vec<blocks::HeaderToSync>,
        authority_set_change: Option<blocks::AuthoritySetChange>,
    ) -> CesealResult<pb::SyncedTo> {
        info!(
            range=?(
                headers.first().map(|h| h.header.number),
                headers.last().map(|h| h.header.number)
            ),
            "sync_header",
        );
        self.can_load_chain_state = false;
        let last_header = self
            .runtime_state()?
            .storage_synchronizer
            .sync_header(headers, authority_set_change)
            .map_err(from_display)?;

        Ok(pb::SyncedTo { synced_to: last_header })
    }

    pub(crate) fn dispatch_blocks(
        &mut self,
        mut blocks: Vec<blocks::BlockHeaderWithChanges>,
    ) -> CesealResult<pb::SyncedTo> {
        info!(
            range=?(
                blocks.first().map(|h| h.block_header.number),
                blocks.last().map(|h| h.block_header.number)
            ),
            "dispatch_block",
        );
        let counters = self.runtime_state()?.storage_synchronizer.counters();
        blocks.retain(|b| b.block_header.number >= counters.next_block_number);

        let last_block = blocks
            .last()
            .map(|b| b.block_header.number)
            .unwrap_or(counters.next_block_number - 1);

        let safe_mode_level = self.args.safe_mode_level;

        for block in blocks.into_iter() {
            info!(block = block.block_header.number, "Dispatching");
            let state = self.runtime_state()?;
            {
                let mut chain_storage = state.chain_storage.write();
                let drop_proofs = safe_mode_level > 1;
                state
                    .storage_synchronizer
                    .feed_block(&block, chain_storage.inner_mut(), drop_proofs)
                    .map_err(from_display)?;
            }
            if safe_mode_level > 0 {
                continue
            }
            info!("State synced");
            state.purge_mq();
            let block_number = block.block_header.number;

            self.handle_inbound_messages(block_number)?;

            if let Err(e) = self.maybe_take_checkpoint() {
                error!("Failed to take checkpoint: {:?}", e);
            }
        }
        Ok(pb::SyncedTo { synced_to: last_block })
    }

    fn maybe_take_checkpoint(&mut self) -> anyhow::Result<()> {
        if !self.args.enable_checkpoint {
            return Ok(())
        }
        if self.last_checkpoint.elapsed().as_secs() < self.args.checkpoint_interval {
            return Ok(())
        }
        self.take_checkpoint()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn init_runtime(
        &mut self,
        genesis: blocks::GenesisBlockInfo,
        genesis_state: blocks::StorageState,
        operator: Option<chain::AccountId>,
        debug_set_key: ::core::option::Option<Vec<u8>>,
        attestation_provider: ::core::option::Option<AttestationProvider>,
    ) -> CesealResult<pb::InitRuntimeResponse> {
        if self.system.is_some() {
            return Err(from_display("Runtime already initialized"))
        }

        info!("Initializing runtime");
        info!("operator      : {operator:?}");
        info!("ra_provider   : {attestation_provider:?}");
        info!("debug_set_key : {debug_set_key:?}");

        trace!("genesis block: {genesis:?}");
        debug!("genesis state len: {:?}", genesis_state.len());

        // load chain genesis
        let genesis_block_hash = genesis.block_header.hash();
        info!("genesis block hash: {genesis_block_hash:?}");
        let chain_storage = ChainStorage::from_pairs(genesis_state.into_iter());
        info!("Genesis state loaded: root={:?}", chain_storage.root());

        // load identity
        let rt_data = if let Some(raw_key) = debug_set_key {
            let priv_key = sr25519::Pair::from_seed_slice(&raw_key).map_err(from_debug)?;
            self.init_runtime_data(genesis_block_hash, Some(priv_key)).map_err(from_debug)?
        } else {
            self.init_runtime_data(genesis_block_hash, None).map_err(from_debug)?
        };
        self.dev_mode = rt_data.dev_mode;
        self.trusted_sk = rt_data.trusted_sk;

        self.attestation_provider = attestation_provider;
        info!("attestation_provider: {:?}", self.attestation_provider);

        if self.dev_mode && self.attestation_provider.is_some() {
            return Err(from_display("RA is disallowed when debug_set_key is enabled"))
        }

        self.platform.quote_test(self.attestation_provider).map_err(from_debug)?;

        let (identity_key, ecdh_key) = rt_data.decode_keys();

        let ecdsa_pk = identity_key.public();
        let ecdsa_hex_pk = hex::encode(ecdsa_pk);
        info!("Identity pubkey: {:?}", ecdsa_hex_pk);

        // derive ecdh key
        let ecdh_pubkey = ces_types::EcdhPublicKey(ecdh_key.public());
        let ecdh_hex_pk = hex::encode(ecdh_pubkey.0.as_ref());
        info!("ECDH pubkey: {:?}", ecdh_hex_pk);

        // Measure machine score
        let cpu_core_num: u32 = self.platform.cpu_core_num();
        info!("CPU cores: {}", cpu_core_num);

        let cpu_feature_level: u32 = self.platform.cpu_feature_level();
        info!("CPU feature level: {}", cpu_feature_level);

        // Initialize bridge
        let mut light_client = LightValidation::new();
        let main_bridge = light_client
            .initialize_bridge(genesis.block_header.clone(), genesis.authority_set, genesis.proof)
            .expect("Bridge initialize failed");

        let storage_synchronizer = Synchronizer::new_solochain(light_client, main_bridge);
        let send_mq = MessageSendQueue::default();
        let recv_mq = MessageDispatcher::default();

        // In parachain mode the state root is stored in parachain header which isn't passed in here.
        // The storage root would be checked at the time each block being synced in(where the storage
        // being patched) and ceseal doesn't read any data from the chain storage until the first
        // block being synced in. So it's OK to skip the check here.
        {
            let this_root = *chain_storage.root();
            if this_root != genesis.block_header.state_root {
                error!(
                    "Genesis state root mismatch, required in header: {:?}, actual: {:?}",
                    genesis.block_header.state_root, this_root,
                );
                return Err(from_display("state root mismatch"))
            }
        }

        let mut runtime_state = RuntimeState {
            send_mq,
            recv_mq,
            storage_synchronizer,
            chain_storage: Arc::new(parking_lot::RwLock::new(chain_storage)),
            genesis_block_hash,
        };

        let system = system::System::new(
            self.platform.clone(),
            self.dev_mode,
            self.args.sealing_path.clone(),
            self.args.storage_path.clone(),
            identity_key,
            ecdh_key,
            &runtime_state.send_mq,
            &mut runtime_state.recv_mq,
            self.args.clone(),
        );

        // Build WorkerRegistrationInfo
        let runtime_info = WorkerRegistrationInfo::<chain::AccountId> {
            version: Self::compat_app_version(),
            machine_id: self.machine_id.clone(),
            pubkey: ecdsa_pk,
            ecdh_pubkey,
            genesis_block_hash,
            features: vec![cpu_core_num, cpu_feature_level],
            operator,
            role: self.args.role.clone(),
        };

        let resp = pb::InitRuntimeResponse::new(runtime_info, genesis_block_hash, ecdsa_pk, ecdh_pubkey, None);
        self.runtime_info = Some(resp.clone());
        self.runtime_state = Some(runtime_state);
        self.system = Some(system);
        Ok(resp)
    }

    fn get_runtime_info(
        &mut self,
        refresh_ra: bool,
        operator: Option<chain::AccountId>,
    ) -> CesealResult<pb::InitRuntimeResponse> {
        // The IdentityKey is considered valid in two situations:
        //
        // 1. It's generated by ceseal thus is safe;
        // 2. It's handovered, but we find out that it was successfully registered as a worker on-chain;
        let validated_identity_key = self.trusted_sk || self.system()?.is_registered();
        let validated_state = self.runtime_state()?.storage_synchronizer.state_validated();

        let reset_operator = operator.is_some();
        if reset_operator {
            self.update_runtime_info(move |info| {
                info.operator = operator;
            });
        }

        let cached_resp = self
            .runtime_info
            .as_mut()
            .ok_or_else(|| from_display("Uninitiated runtime info"))?;

        if let Some(cached_attestation) = &cached_resp.attestation {
            const MAX_ATTESTATION_AGE: u64 = 60 * 60;
            if refresh_ra || reset_operator || now() > cached_attestation.timestamp + MAX_ATTESTATION_AGE {
                cached_resp.attestation = None;
            }
        }

        let allow_attestation = validated_state && (validated_identity_key || self.attestation_provider.is_none());
        info!("validated_identity_key :{validated_identity_key}");
        info!("validated_state        :{validated_state}");
        info!("refresh_ra             :{refresh_ra}");
        info!("reset_operator         :{reset_operator}");
        info!("attestation_provider   :{:?}", self.attestation_provider);
        info!("allow_attestation      :{allow_attestation}");
        // Never generate RA report for a potentially injected identity key
        // else he is able to fake a Secure Worker
        if allow_attestation && cached_resp.attestation.is_none() {
            // We hash the encoded bytes directly
            let runtime_info_hash = sp_core::hashing::blake2_256(&cached_resp.encoded_runtime_info);
            info!("Encoded runtime info");
            info!("{:?}", hex::encode(&cached_resp.encoded_runtime_info));

            let report = create_attestation_report_on(
                &self.platform,
                self.attestation_provider,
                &runtime_info_hash,
                self.args.ra_timeout,
                self.args.ra_max_retries,
            )?;
            cached_resp.attestation = Some(report);
        }

        Ok(cached_resp.clone())
    }

    fn get_egress_messages(&mut self) -> CesealResult<pb::EgressMessages> {
        let messages: Vec<_> = self
            .runtime_state
            .as_ref()
            .map(|state| state.send_mq.all_messages_grouped().into_iter().collect())
            .unwrap_or_default();
        Ok(messages)
    }

    fn handle_inbound_messages(&mut self, block_number: chain::BlockNumber) -> CesealResult<()> {
        let state = self
            .runtime_state
            .as_mut()
            .ok_or_else(|| from_display("Runtime not initialized"))?;
        let system = self.system.as_mut().ok_or_else(|| from_display("Runtime not initialized"))?;

        let chain_storage = state.chain_storage.read();
        // Dispatch events
        let messages = chain_storage
            .mq_messages()
            .map_err(|_| from_display("Can not get mq messages from storage"))?;

        state.recv_mq.reset_local_index();

        let now_ms = chain_storage.timestamp_now();

        let mut block = BlockDispatchContext {
            block_number,
            now_ms,
            storage: chain_storage.borrow(),
            send_mq: &state.send_mq,
            recv_mq: &mut state.recv_mq,
        };

        system.will_process_block(&mut block);

        for message in messages {
            use ces_types::messaging::WorkerEvent;
            macro_rules! log_message {
                ($msg: expr, $t: ident) => {{
                    let event: Result<$t, _> =
                        parity_scale_codec::Decode::decode(&mut &$msg.payload[..]);
                    match event {
                        Ok(event) => {
                            debug!(target: "ces_mq",
                                "mq dispatching message: sender={} dest={:?} payload={:?}",
                                $msg.sender, $msg.destination, event
                            );
                        }
                        Err(_) => {
                            debug!(target: "ces_mq", "mq dispatching message (decode failed): {:?}", $msg);
                        }
                    }
                }};
            }
            if message.destination.path() == &WorkerEvent::topic() {
                log_message!(message, WorkerEvent);
            } else {
                debug!(target: "ces_mq",
                    "mq dispatching message: sender={}, dest={:?}",
                    message.sender, message.destination
                );
            }
            block.recv_mq.dispatch(message);

            system.process_messages(&mut block);
        }
        system.did_process_block(&mut block);

        let n_unhandled = state.recv_mq.clear();
        if n_unhandled > 0 {
            warn!("There are {} unhandled messages dropped", n_unhandled);
        }

        Ok(())
    }

    fn set_endpoint(&mut self, endpoint: String) -> CesealResult<pb::GetEndpointResponse> {
        self.endpoint = Some(endpoint);
        self.sign_endpoint()
    }

    fn sign_endpoint(&mut self) -> CesealResult<pb::GetEndpointResponse> {
        let system = self.system()?;
        let block_time: u64 = system.now_ms;
        let public_key = system.identity_key.public();
        let endpoint = self.endpoint.clone();
        let endpoint_payload = WorkerEndpointPayload { pubkey: public_key, endpoint, signing_time: block_time };
        let signature = self.sign_endpoint_payload(&endpoint_payload)?;
        let resp = pb::GetEndpointResponse::new(Some(endpoint_payload.clone()), Some(signature));
        self.signed_endpoint = Some(resp.clone());
        Ok(resp)
    }

    fn get_endpoint_info(&mut self) -> CesealResult<pb::GetEndpointResponse> {
        if self.endpoint.is_none() {
            info!("Endpoint not found");
            return Ok(pb::GetEndpointResponse::new(None, None))
        }
        match &self.signed_endpoint {
            Some(response) => Ok(response.clone()),
            None => self.sign_endpoint(),
        }
    }

    fn sign_endpoint_payload(&mut self, payload: &WorkerEndpointPayload) -> CesealResult<Vec<u8>> {
        const MAX_PAYLOAD_SIZE: usize = 512;
        let data_to_sign = payload.encode();
        if data_to_sign.len() > MAX_PAYLOAD_SIZE {
            return Err(from_display("Endpoints too large"))
        }
        let wrapped_data = wrap_content_to_sign(&data_to_sign, SignedContentType::EndpointInfo);
        let signature = self.system()?.identity_key.clone().sign(&wrapped_data).encode();
        Ok(signature)
    }

    fn get_master_key_apply(&mut self) -> CesealResult<pb::GetMasterKeyApplyResponse> {
        let system = self.system()?;
        let block_time: u64 = system.now_ms;
        let pubkey = system.identity_key.public();
        let ecdh_pubkey = sp_core::sr25519::Public::from_raw(system.ecdh_key.public());
        let payload = MasterKeyApplyPayload { pubkey, ecdh_pubkey, signing_time: block_time };
        let data_to_sign = payload.encode();
        let wrapped_data = wrap_content_to_sign(&data_to_sign, SignedContentType::MasterKeyApply);
        let signature = system.identity_key.sign(&wrapped_data).encode();
        Ok(pb::GetMasterKeyApplyResponse::new(payload, signature))
    }

    pub fn load_chain_state(&mut self, block: chain::BlockNumber, storage: StorageState) -> anyhow::Result<()> {
        if !self.can_load_chain_state {
            anyhow::bail!("Can not load chain state");
        }
        if block == 0 {
            anyhow::bail!("Can not load chain state from block 0");
        }
        let Some(system) = &mut self.system else {
            anyhow::bail!("System is uninitialized");
        };
        let Some(state) = &mut self.runtime_state else {
            anyhow::bail!("Runtime is uninitialized");
        };
        let chain_storage = ChainStorage::from_pairs(storage.into_iter());
        if chain_storage.is_worker_registered(&system.identity_key.public()) {
            anyhow::bail!("Failed to load state: the worker is already registered at block {block}",);
        }
        state
            .storage_synchronizer
            .assume_at_block(block)
            .context("Failed to set synchronizer state")?;
        state.chain_storage = Arc::new(parking_lot::RwLock::new(chain_storage));
        system.genesis_block = block;
        self.can_load_chain_state = false;
        Ok(())
    }

    pub fn stop(&self, remove_checkpoints: bool) -> CesealResult<()> {
        info!("Requested to stop remove_checkpoints={remove_checkpoints}");
        if remove_checkpoints {
            crate::maybe_remove_checkpoints(&self.args.storage_path);
        }
        std::process::abort()
    }

    fn load_storage_proof(&mut self, proof: Vec<Vec<u8>>) -> CesealResult<()> {
        if self.args.safe_mode_level < 2 {
            return Err(from_display("Can not load storage proof when safe_mode_level < 2"))
        }
        let mut chain_storage = self.runtime_state()?.chain_storage.write();
        chain_storage.inner_mut().load_proof(proof);
        Ok(())
    }

    fn get_worker_key_challenge(
        &mut self,
        block_number: chain::BlockNumber,
        now: u64,
    ) -> HandoverChallenge<chain::BlockNumber> {
        let sgx_target_info = if self.dev_mode {
            vec![]
        } else {
            let my_target_info = sgx_api_lite::target_info().unwrap();
            sgx_api_lite::encode(&my_target_info).to_vec()
        };
        let challenge = HandoverChallenge {
            sgx_target_info,
            block_number,
            now,
            dev_mode: self.dev_mode,
            nonce: crate::generate_random_info(),
        };
        self.handover_last_challenge = Some(challenge.clone());
        challenge
    }

    pub fn verify_worker_key_challenge(&mut self, challenge: &HandoverChallenge<chain::BlockNumber>) -> bool {
        return self.handover_last_challenge.take().as_ref() == Some(challenge)
    }
}
