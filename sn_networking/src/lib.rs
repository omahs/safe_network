// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;

mod bootstrap;
mod circular_vec;
mod cmd;
mod driver;
mod error;
mod event;
mod external_address;
mod log_markers;
#[cfg(feature = "open-metrics")]
mod metrics;
#[cfg(feature = "open-metrics")]
mod metrics_service;
mod network_discovery;
mod record_store;
mod record_store_api;
mod relay_manager;
mod replication_fetcher;
mod spends;
pub mod target_arch;
mod transfers;
mod transport;

use cmd::LocalSwarmCmd;
use xor_name::XorName;

// re-export arch dependent deps for use in the crate, or above
pub use target_arch::{interval, sleep, spawn, Instant, Interval};

pub use self::{
    cmd::{NodeIssue, SwarmLocalState},
    driver::{
        GetRecordCfg, NetworkBuilder, PutRecordCfg, SwarmDriver, VerificationKind, MAX_PACKET_SIZE,
    },
    error::{GetRecordError, NetworkError},
    event::{MsgResponder, NetworkEvent},
    record_store::{calculate_cost_for_records, NodeRecordStore},
    transfers::{get_raw_signed_spends_from_record, get_signed_spend_from_record},
};

use self::{cmd::NetworkSwarmCmd, error::Result};
use backoff::{Error as BackoffError, ExponentialBackoff};
use futures::future::select_all;
use libp2p::{
    identity::Keypair,
    kad::{KBucketDistance, KBucketKey, Quorum, Record, RecordKey},
    multiaddr::Protocol,
    request_response::OutboundFailure,
    Multiaddr, PeerId,
};
use rand::Rng;
use sn_protocol::{
    error::Error as ProtocolError,
    messages::{ChunkProof, Cmd, Nonce, Query, QueryResponse, Request, Response},
    storage::{RecordType, RetryStrategy},
    NetworkAddress, PrettyPrintKBucketKey, PrettyPrintRecordKey, CLOSE_GROUP_SIZE,
};
use sn_transfers::{MainPubkey, NanoTokens, PaymentQuote, QuotingMetrics};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tokio::time::Duration;

/// The type of quote for a selected payee.
pub type PayeeQuote = (PeerId, MainPubkey, PaymentQuote);

/// The count of peers that will be considered as close to a record target,
/// that a replication of the record shall be sent/accepted to/by the peer.
pub const REPLICATION_PEERS_COUNT: usize = CLOSE_GROUP_SIZE + 2;

/// Majority of a given group (i.e. > 1/2).
#[inline]
pub const fn close_group_majority() -> usize {
    // Calculate the majority of the close group size by dividing it by 2 and adding 1.
    // This ensures that the majority is always greater than half.
    CLOSE_GROUP_SIZE / 2 + 1
}

/// Max duration to wait for verification.
const MAX_WAIT_BEFORE_READING_A_PUT: Duration = Duration::from_millis(750);
/// Min duration to wait for verification
const MIN_WAIT_BEFORE_READING_A_PUT: Duration = Duration::from_millis(300);

/// Sort the provided peers by their distance to the given `NetworkAddress`.
/// Return with the closest expected number of entries if has.
pub fn sort_peers_by_address<'a>(
    peers: &'a Vec<PeerId>,
    address: &NetworkAddress,
    expected_entries: usize,
) -> Result<Vec<&'a PeerId>> {
    sort_peers_by_key(peers, &address.as_kbucket_key(), expected_entries)
}

/// Sort the provided peers by their distance to the given `KBucketKey`.
/// Return with the closest expected number of entries if has.
pub fn sort_peers_by_key<'a, T>(
    peers: &'a Vec<PeerId>,
    key: &KBucketKey<T>,
    expected_entries: usize,
) -> Result<Vec<&'a PeerId>> {
    // Check if there are enough peers to satisfy the request.
    // bail early if that's not the case
    if CLOSE_GROUP_SIZE > peers.len() {
        warn!("Not enough peers in the k-bucket to satisfy the request");
        return Err(NetworkError::NotEnoughPeers {
            found: peers.len(),
            required: CLOSE_GROUP_SIZE,
        });
    }

    // Create a vector of tuples where each tuple is a reference to a peer and its distance to the key.
    // This avoids multiple computations of the same distance in the sorting process.
    let mut peer_distances: Vec<(&PeerId, KBucketDistance)> = Vec::with_capacity(peers.len());

    for peer_id in peers {
        let addr = NetworkAddress::from_peer(*peer_id);
        let distance = key.distance(&addr.as_kbucket_key());
        peer_distances.push((peer_id, distance));
    }

    // Sort the vector of tuples by the distance.
    peer_distances.sort_by(|a, b| a.1.cmp(&b.1));

    // Collect the sorted peers into a new vector.
    let sorted_peers: Vec<_> = peer_distances
        .into_iter()
        .take(expected_entries)
        .map(|(peer_id, _)| peer_id)
        .collect();

    Ok(sorted_peers)
}

#[derive(Clone, Debug)]
/// API to interact with the underlying Swarm
pub struct Network {
    inner: Arc<NetworkInner>,
}

/// The actual implementation of the Network. The other is just a wrapper around this, so that we don't expose
/// the Arc from the interface.
#[derive(Debug)]
struct NetworkInner {
    network_swarm_cmd_sender: mpsc::Sender<NetworkSwarmCmd>,
    local_swarm_cmd_sender: mpsc::Sender<LocalSwarmCmd>,
    peer_id: PeerId,
    root_dir_path: PathBuf,
    keypair: Keypair,
}

impl Network {
    pub fn new(
        network_swarm_cmd_sender: mpsc::Sender<NetworkSwarmCmd>,
        local_swarm_cmd_sender: mpsc::Sender<LocalSwarmCmd>,
        peer_id: PeerId,
        root_dir_path: PathBuf,
        keypair: Keypair,
    ) -> Self {
        Self {
            inner: Arc::new(NetworkInner {
                network_swarm_cmd_sender,
                local_swarm_cmd_sender,
                peer_id,
                root_dir_path,
                keypair,
            }),
        }
    }

    /// Returns the `PeerId` of the instance.
    pub fn peer_id(&self) -> PeerId {
        self.inner.peer_id
    }

    /// Returns the `Keypair` of the instance.
    pub fn keypair(&self) -> &Keypair {
        &self.inner.keypair
    }

    /// Returns the root directory path of the instance.
    pub fn root_dir_path(&self) -> &PathBuf {
        &self.inner.root_dir_path
    }

    /// Get the sender to send a `NetworkSwarmCmd` to the underlying `Swarm`.
    pub(crate) fn network_swarm_cmd_sender(&self) -> &mpsc::Sender<NetworkSwarmCmd> {
        &self.inner.network_swarm_cmd_sender
    }
    /// Get the sender to send a `LocalSwarmCmd` to the underlying `Swarm`.
    pub(crate) fn local_swarm_cmd_sender(&self) -> &mpsc::Sender<LocalSwarmCmd> {
        &self.inner.local_swarm_cmd_sender
    }

    /// Signs the given data with the node's keypair.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        self.keypair().sign(msg).map_err(NetworkError::from)
    }

    /// Verifies a signature for the given data and the node's public key.
    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        self.keypair().public().verify(msg, sig)
    }

    /// Returns the protobuf serialised PublicKey to allow messaging out for share.
    pub fn get_pub_key(&self) -> Vec<u8> {
        self.keypair().public().encode_protobuf()
    }

    /// Dial the given peer at the given address.
    /// This function will only be called for the bootstrap nodes.
    pub async fn dial(&self, addr: Multiaddr) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.send_network_swarm_cmd(NetworkSwarmCmd::Dial { addr, sender });
        receiver.await?
    }

    /// Returns the closest peers to the given `XorName`, sorted by their distance to the xor_name.
    /// Excludes the client's `PeerId` while calculating the closest peers.
    pub async fn client_get_closest_peers(&self, key: &NetworkAddress) -> Result<Vec<PeerId>> {
        self.get_closest_peers(key, true).await
    }

    /// Returns the closest peers to the given `NetworkAddress`, sorted by their distance to the key.
    ///
    /// Includes our node's `PeerId` while calculating the closest peers.
    pub async fn node_get_closest_peers(&self, key: &NetworkAddress) -> Result<Vec<PeerId>> {
        self.get_closest_peers(key, false).await
    }

    /// Returns a map where each key is the ilog2 distance of that Kbucket and each value is a vector of peers in that
    /// bucket.
    /// Does not include self
    pub async fn get_kbuckets(&self) -> Result<BTreeMap<u32, Vec<PeerId>>> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetKBuckets { sender });
        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Returns all the PeerId from all the KBuckets from our local Routing Table
    /// Also contains our own PeerId.
    pub async fn get_closest_k_value_local_peers(&self) -> Result<Vec<PeerId>> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetClosestKLocalPeers { sender });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Get the Chunk existence proof from the close nodes to the provided chunk address.
    pub async fn verify_chunk_existence(
        &self,
        chunk_address: NetworkAddress,
        nonce: Nonce,
        expected_proof: ChunkProof,
        quorum: Quorum,
        retry_strategy: Option<RetryStrategy>,
    ) -> Result<()> {
        let mut total_attempts = 1;
        total_attempts += retry_strategy
            .map(|strategy| strategy.get_count())
            .unwrap_or(0);

        let pretty_key = PrettyPrintRecordKey::from(&chunk_address.to_record_key()).into_owned();
        let expected_n_verified = get_quorum_value(&quorum);

        let mut close_nodes = Vec::new();
        let mut retry_attempts = 0;
        while retry_attempts < total_attempts {
            // the check should happen before incrementing retry_attempts
            if retry_attempts % 2 == 0 {
                // Do not query the closest_peers during every re-try attempt.
                // The close_nodes don't change often and the previous set of close_nodes might be taking a while to write
                // the Chunk, so query them again incase of a failure.
                close_nodes = self.get_closest_peers(&chunk_address, true).await?;
            }
            retry_attempts += 1;
            info!(
                "Getting ChunkProof for {pretty_key:?}. Attempts: {retry_attempts:?}/{total_attempts:?}",
            );

            let request = Request::Query(Query::GetChunkExistenceProof {
                key: chunk_address.clone(),
                nonce,
            });
            let responses = self
                .send_and_get_responses(&close_nodes, &request, true)
                .await;
            let n_verified = responses
                .into_iter()
                .filter_map(|(peer, resp)| {
                    if let Ok(Response::Query(QueryResponse::GetChunkExistenceProof(Ok(proof)))) =
                        resp
                    {
                        if expected_proof.verify(&proof) {
                            debug!("Got a valid ChunkProof from {peer:?}");
                            Some(())
                        } else {
                            warn!("Failed to verify the ChunkProof from {peer:?}. The chunk might have been tampered?");
                            None
                        }
                    } else {
                        debug!("Did not get a valid response for the ChunkProof from {peer:?}");
                        None
                    }
                })
                .count();
            debug!("Got {n_verified} verified chunk existence proofs for chunk_address {chunk_address:?}");

            if n_verified >= expected_n_verified {
                return Ok(());
            }
            warn!("The obtained {n_verified} verified proofs did not match the expected {expected_n_verified} verified proofs");
            // Sleep to avoid firing queries too close to even choke the nodes further.
            let waiting_time = if retry_attempts == 1 {
                MIN_WAIT_BEFORE_READING_A_PUT
            } else {
                MIN_WAIT_BEFORE_READING_A_PUT + MIN_WAIT_BEFORE_READING_A_PUT
            };
            sleep(waiting_time).await;
        }

        Err(NetworkError::FailedToVerifyChunkProof(
            chunk_address.clone(),
        ))
    }

    /// Get the store costs from the majority of the closest peers to the provided RecordKey.
    /// Record already exists will have a cost of zero to be returned.
    ///
    /// Ignore the quote from any peers from `ignore_peers`. This is useful if we want to repay a different PeerId
    /// on failure.
    pub async fn get_store_costs_from_network(
        &self,
        record_address: NetworkAddress,
        ignore_peers: Vec<PeerId>,
    ) -> Result<PayeeQuote> {
        // The requirement of having at least CLOSE_GROUP_SIZE
        // close nodes will be checked internally automatically.
        let close_nodes = self.get_closest_peers(&record_address, true).await?;

        let request = Request::Query(Query::GetStoreCost(record_address.clone()));
        let responses = self
            .send_and_get_responses(&close_nodes, &request, true)
            .await;

        // loop over responses, generating an average fee and storing all responses along side
        let mut all_costs = vec![];
        let mut all_quotes = vec![];
        for response in responses.into_values().flatten() {
            info!(
                "StoreCostReq for {record_address:?} received response: {:?}",
                response
            );
            match response {
                Response::Query(QueryResponse::GetStoreCost {
                    quote: Ok(quote),
                    payment_address,
                    peer_address,
                }) => {
                    // Check the quote itself is valid.
                    if quote.cost.as_nano()
                        != calculate_cost_for_records(quote.quoting_metrics.close_records_stored)
                    {
                        warn!("Received invalid quote from {peer_address:?}, {quote:?}");
                        continue;
                    }

                    all_costs.push((peer_address.clone(), payment_address, quote.clone()));
                    all_quotes.push((peer_address, quote));
                }
                Response::Query(QueryResponse::GetStoreCost {
                    quote: Err(ProtocolError::RecordExists(_)),
                    payment_address,
                    peer_address,
                }) => {
                    all_costs.push((peer_address, payment_address, PaymentQuote::zero()));
                }
                _ => {
                    error!("Non store cost response received,  was {:?}", response);
                }
            }
        }

        for peer_id in close_nodes.iter() {
            let request = Request::Cmd(Cmd::QuoteVerification {
                target: NetworkAddress::from_peer(*peer_id),
                quotes: all_quotes.clone(),
            });

            self.send_req_ignore_reply(request, *peer_id);
        }

        // Sort all_costs by the NetworkAddress proximity to record_address
        all_costs.sort_by(|(peer_address_a, _, _), (peer_address_b, _, _)| {
            record_address
                .distance(peer_address_a)
                .cmp(&record_address.distance(peer_address_b))
        });
        let ignore_peers = ignore_peers
            .into_iter()
            .map(NetworkAddress::from_peer)
            .collect::<BTreeSet<_>>();

        // Ensure we dont have any further out nodes than `close_group_majority()`
        // This should ensure that if we didnt get all responses from close nodes,
        // we're less likely to be paying a node that is not in the CLOSE_GROUP
        //
        // Also filter out the peers.
        let all_costs = all_costs
            .into_iter()
            .filter(|(peer_address, ..)| !ignore_peers.contains(peer_address))
            .take(close_group_majority())
            .collect();

        get_fees_from_store_cost_responses(all_costs)
    }

    /// Get register from network.
    /// Due to the nature of the p2p network, it's not guaranteed there is only one version
    /// exists in the network all the time.
    /// The scattering of the register will be more like `ring layered`.
    /// Meanwhile, `kad::get_record` will terminate with first majority copies returned,
    /// which has the risk of returning with old versions.
    /// So, to improve the accuracy, query closest_peers first, then fetch registers
    /// And merge them if they are with different content.
    pub async fn get_register_record_from_network(
        &self,
        key: RecordKey,
    ) -> Result<HashMap<XorName, Record>> {
        let record_address = NetworkAddress::from_record_key(&key);
        // The requirement of having at least CLOSE_GROUP_SIZE
        // close nodes will be checked internally automatically.
        let close_nodes = self.get_closest_peers(&record_address, true).await?;

        let self_address = NetworkAddress::from_peer(self.peer_id());
        let request = Request::Query(Query::GetRegisterRecord {
            requester: self_address,
            key: record_address.clone(),
        });
        let responses = self
            .send_and_get_responses(&close_nodes, &request, true)
            .await;

        // loop over responses, collecting all fetched register records
        let mut all_register_copies = HashMap::new();
        for response in responses.into_values().flatten() {
            match response {
                Response::Query(QueryResponse::GetRegisterRecord(Ok((holder, content)))) => {
                    let register_record = Record::new(key.clone(), content.to_vec());
                    let content_hash = XorName::from_content(&register_record.value);
                    debug!(
                        "RegisterRecordReq of {record_address:?} received register of version {content_hash:?} from {holder:?}"
                    );
                    let _ = all_register_copies.insert(content_hash, register_record);
                }
                _ => {
                    error!(
                        "RegisterRecordReq of {record_address:?} received error response, was {:?}",
                        response
                    );
                }
            }
        }

        Ok(all_register_copies)
    }

    /// Get a record from the network
    /// This differs from non-wasm32 builds as no retries are applied
    #[cfg(target_arch = "wasm32")]
    pub async fn get_record_from_network(
        &self,
        key: RecordKey,
        cfg: &GetRecordCfg,
    ) -> Result<Record> {
        let pretty_key = PrettyPrintRecordKey::from(&key);
        info!("Getting record from network of {pretty_key:?}. with cfg {cfg:?}",);
        let (sender, receiver) = oneshot::channel();
        self.send_network_swarm_cmd(NetworkSwarmCmd::GetNetworkRecord {
            key: key.clone(),
            sender,
            cfg: cfg.clone(),
        });
        let result = receiver.await.map_err(|e| {
            error!("When fetching record {pretty_key:?}, encountered a channel error {e:?}");
            NetworkError::InternalMsgChannelDropped
        })?;

        result.map_err(NetworkError::from)
    }

    /// Get the Record from the network
    /// Carry out re-attempts if required
    /// In case a target_record is provided, only return when fetched target.
    /// Otherwise count it as a failure when all attempts completed.
    ///
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn get_record_from_network(
        &self,
        key: RecordKey,
        cfg: &GetRecordCfg,
    ) -> Result<Record> {
        let retry_duration = cfg.retry_strategy.map(|strategy| strategy.get_duration());
        backoff::future::retry(
            ExponentialBackoff {
                // None sets a random duration, but we'll be terminating with a BackoffError::Permanent, so retry will
                // be disabled.
                max_elapsed_time: retry_duration,
                ..Default::default()
            },
            || async {
                let pretty_key = PrettyPrintRecordKey::from(&key);
                info!("Getting record from network of {pretty_key:?}. with cfg {cfg:?}",);
                let (sender, receiver) = oneshot::channel();
                self.send_network_swarm_cmd(NetworkSwarmCmd::GetNetworkRecord {
                    key: key.clone(),
                    sender,
                    cfg: cfg.clone(),
                });
                let result = receiver.await.map_err(|e| {
                error!("When fetching record {pretty_key:?}, encountered a channel error {e:?}");
                NetworkError::InternalMsgChannelDropped
            }).map_err(|err| BackoffError::Transient { err,  retry_after: None })?;

                // log the results
                match &result {
                    Ok(_) => {
                        info!("Record returned: {pretty_key:?}.");
                    }
                    Err(GetRecordError::RecordDoesNotMatch(_)) => {
                        warn!("The returned record does not match target {pretty_key:?}.");
                    }
                    Err(GetRecordError::NotEnoughCopies { expected, got, .. }) => {
                        warn!("Not enough copies ({got}/{expected}) found yet for {pretty_key:?}.");
                    }
                    // libp2p RecordNotFound does mean no holders answered.
                    // it does not actually mean the record does not exist.
                    // just that those asked did not have it
                    Err(GetRecordError::RecordNotFound) => {
                        warn!("No holder of record '{pretty_key:?}' found.");
                    }
                    Err(GetRecordError::SplitRecord { .. }) => {
                        error!("Encountered a split record for {pretty_key:?}.");
                    }
                    Err(GetRecordError::QueryTimeout) => {
                        error!("Encountered query timeout for {pretty_key:?}.");
                    }
                };

                // if we don't want to retry, throw permanent error
                if cfg.retry_strategy.is_none() {
                    if let Err(e) = result {
                        return Err(BackoffError::Permanent(NetworkError::from(e)));
                    }
                }
                if result.is_err() {
                    debug!("Getting record from network of {pretty_key:?} via backoff...");
                }
                result.map_err(|err| BackoffError::Transient {
                    err: NetworkError::from(err),
                    retry_after: None,
                })
            },
        )
        .await
    }

    /// Get the cost of storing the next record from the network
    pub async fn get_local_storecost(
        &self,
        key: RecordKey,
    ) -> Result<(NanoTokens, QuotingMetrics)> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetLocalStoreCost { key, sender });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Notify the node receicced a payment.
    pub fn notify_payment_received(&self) {
        self.send_local_swarm_cmd(LocalSwarmCmd::PaymentReceived);
    }

    /// Get `Record` from the local RecordStore
    pub async fn get_local_record(&self, key: &RecordKey) -> Result<Option<Record>> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetLocalRecord {
            key: key.clone(),
            sender,
        });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Whether the target peer is considered blacklisted by self
    pub async fn is_peer_shunned(&self, target: NetworkAddress) -> Result<bool> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::IsPeerShunned { target, sender });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Put `Record` to network
    /// Optionally verify the record is stored after putting it to network
    /// If verify is on, retry multiple times within MAX_PUT_RETRY_DURATION duration.
    pub async fn put_record(&self, record: Record, cfg: &PutRecordCfg) -> Result<()> {
        let pretty_key = PrettyPrintRecordKey::from(&record.key);

        // Here we only retry after a failed validation.
        // So a long validation time will limit the number of PUT retries we attempt here.
        let retry_duration = cfg.retry_strategy.map(|strategy| strategy.get_duration());
        backoff::future::retry(
            ExponentialBackoff {
                // None sets a random duration, but we'll be terminating with a BackoffError::Permanent, so retry will
                // be disabled.
            max_elapsed_time: retry_duration,
            ..Default::default()
        }, || async {

            info!(
                "Attempting to PUT record with key: {pretty_key:?} to network, with cfg {cfg:?}, retrying via backoff..."
            );
            self.put_record_once(record.clone(), cfg).await.map_err(|err|
            {
                warn!("Failed to PUT record with key: {pretty_key:?} to network (retry via backoff) with error: {err:?}");

                if cfg.retry_strategy.is_some() {
                    BackoffError::Transient { err, retry_after: None }
                } else {
                    BackoffError::Permanent(err)
                }

            })
        }).await
    }

    async fn put_record_once(&self, record: Record, cfg: &PutRecordCfg) -> Result<()> {
        let record_key = record.key.clone();
        let pretty_key = PrettyPrintRecordKey::from(&record_key);
        info!(
            "Putting record of {} - length {:?} to network",
            pretty_key,
            record.value.len()
        );

        // Waiting for a response to avoid flushing to network too quick that causing choke
        let (sender, receiver) = oneshot::channel();
        if let Some(put_record_to_peers) = &cfg.use_put_record_to {
            self.send_network_swarm_cmd(NetworkSwarmCmd::PutRecordTo {
                peers: put_record_to_peers.clone(),
                record: record.clone(),
                sender,
                quorum: cfg.put_quorum,
            });
        } else {
            self.send_network_swarm_cmd(NetworkSwarmCmd::PutRecord {
                record: record.clone(),
                sender,
                quorum: cfg.put_quorum,
            });
        }

        let response = receiver.await?;

        if let Some((verification_kind, get_cfg)) = &cfg.verification {
            // Generate a random duration between MAX_WAIT_BEFORE_READING_A_PUT and MIN_WAIT_BEFORE_READING_A_PUT
            let wait_duration = rand::thread_rng()
                .gen_range(MIN_WAIT_BEFORE_READING_A_PUT..MAX_WAIT_BEFORE_READING_A_PUT);
            // Small wait before we attempt to verify.
            // There will be `re-attempts` to be carried out within the later step anyway.
            sleep(wait_duration).await;
            debug!("Attempting to verify {pretty_key:?} after we've slept for {wait_duration:?}");

            // Verify the record is stored, requiring re-attempts
            if let VerificationKind::ChunkProof {
                expected_proof,
                nonce,
            } = verification_kind
            {
                self.verify_chunk_existence(
                    NetworkAddress::from_record_key(&record_key),
                    *nonce,
                    expected_proof.clone(),
                    get_cfg.get_quorum,
                    get_cfg.retry_strategy,
                )
                .await?;
            } else {
                match self
                    .get_record_from_network(record.key.clone(), get_cfg)
                    .await
                {
                    Ok(_) => {
                        debug!("Record {pretty_key:?} verified to be stored.");
                    }
                    Err(NetworkError::GetRecordError(GetRecordError::RecordNotFound)) => {
                        warn!("Record {pretty_key:?} not found after PUT, either rejected or not yet stored by nodes when we asked");
                        return Err(NetworkError::RecordNotStoredByNodes(
                            NetworkAddress::from_record_key(&record_key),
                        ));
                    }
                    Err(e) => {
                        debug!(
                            "Failed to verify record {pretty_key:?} to be stored with error: {e:?}"
                        );
                        return Err(e);
                    }
                }
            }
        }
        response
    }

    /// Notify ReplicationFetch a fetch attempt is completed.
    /// (but it won't trigger any real writes to disk, say fetched an old version of register)
    pub fn notify_fetch_completed(&self, key: RecordKey, record_type: RecordType) {
        self.send_local_swarm_cmd(LocalSwarmCmd::FetchCompleted((key, record_type)))
    }

    /// Put `Record` to the local RecordStore
    /// Must be called after the validations are performed on the Record
    pub fn put_local_record(&self, record: Record) {
        debug!(
            "Writing Record locally, for {:?} - length {:?}",
            PrettyPrintRecordKey::from(&record.key),
            record.value.len()
        );
        self.send_local_swarm_cmd(LocalSwarmCmd::PutVerifiedLocalRecord { record })
    }

    /// Returns true if a RecordKey is present locally in the RecordStore
    pub async fn is_record_key_present_locally(&self, key: &RecordKey) -> Result<bool> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::RecordStoreHasKey {
            key: key.clone(),
            sender,
        });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Returns the Addresses of all the locally stored Records
    pub async fn get_all_local_record_addresses(
        &self,
    ) -> Result<HashMap<NetworkAddress, RecordType>> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetAllLocalRecordAddresses { sender });

        receiver
            .await
            .map_err(|_e| NetworkError::InternalMsgChannelDropped)
    }

    /// Send `Request` to the given `PeerId` and await for the response. If `self` is the recipient,
    /// then the `Request` is forwarded to itself and handled, and a corresponding `Response` is created
    /// and returned to itself. Hence the flow remains the same and there is no branching at the upper
    /// layers.
    ///
    /// If an outbound issue is raised, we retry once more to send the request before returning an error.
    pub async fn send_request(&self, req: Request, peer: PeerId) -> Result<Response> {
        let (sender, receiver) = oneshot::channel();
        self.send_network_swarm_cmd(NetworkSwarmCmd::SendRequest {
            req: req.clone(),
            peer,
            sender: Some(sender),
        });
        let mut r = receiver.await?;

        if let Err(error) = &r {
            error!("Error in response: {:?}", error);

            match error {
                NetworkError::OutboundError(OutboundFailure::Io(_))
                | NetworkError::OutboundError(OutboundFailure::ConnectionClosed) => {
                    warn!(
                        "Outbound failed for {req:?} .. {error:?}, redialing once and reattempting"
                    );
                    let (sender, receiver) = oneshot::channel();

                    debug!("Reattempting to send_request {req:?} to {peer:?}");
                    self.send_network_swarm_cmd(NetworkSwarmCmd::SendRequest {
                        req,
                        peer,
                        sender: Some(sender),
                    });

                    r = receiver.await?;
                }
                _ => {
                    // If the record is found, we should log the error and continue
                    warn!("Error in response: {:?}", error);
                }
            }
        }

        r
    }

    /// Send `Request` to the given `PeerId` and do _not_ await a response here.
    /// Instead the Response will be handled by the common `response_handler`
    pub fn send_req_ignore_reply(&self, req: Request, peer: PeerId) {
        let swarm_cmd = NetworkSwarmCmd::SendRequest {
            req,
            peer,
            sender: None,
        };
        self.send_network_swarm_cmd(swarm_cmd)
    }

    /// Send a `Response` through the channel opened by the requester.
    pub fn send_response(&self, resp: Response, channel: MsgResponder) {
        self.send_network_swarm_cmd(NetworkSwarmCmd::SendResponse { resp, channel })
    }

    /// Return a `SwarmLocalState` with some information obtained from swarm's local state.
    pub async fn get_swarm_local_state(&self) -> Result<SwarmLocalState> {
        let (sender, receiver) = oneshot::channel();
        self.send_local_swarm_cmd(LocalSwarmCmd::GetSwarmLocalState(sender));
        let state = receiver.await?;
        Ok(state)
    }

    pub fn trigger_interval_replication(&self) {
        self.send_local_swarm_cmd(LocalSwarmCmd::TriggerIntervalReplication)
    }

    pub fn record_node_issues(&self, peer_id: PeerId, issue: NodeIssue) {
        self.send_local_swarm_cmd(LocalSwarmCmd::RecordNodeIssue { peer_id, issue });
    }

    pub fn historical_verify_quotes(&self, quotes: Vec<(PeerId, PaymentQuote)>) {
        self.send_local_swarm_cmd(LocalSwarmCmd::QuoteVerification { quotes });
    }

    pub fn trigger_unrelevant_record_cleanup(&self) {
        self.send_local_swarm_cmd(LocalSwarmCmd::TriggerUnrelevantRecordCleanup)
    }

    /// Helper to send NetworkSwarmCmd
    fn send_network_swarm_cmd(&self, cmd: NetworkSwarmCmd) {
        send_network_swarm_cmd(self.network_swarm_cmd_sender().clone(), cmd);
    }
    /// Helper to send LocalSwarmCmd
    fn send_local_swarm_cmd(&self, cmd: LocalSwarmCmd) {
        send_local_swarm_cmd(self.local_swarm_cmd_sender().clone(), cmd);
    }

    /// Returns the closest peers to the given `XorName`, sorted by their distance to the xor_name.
    /// If `client` is false, then include `self` among the `closest_peers`
    pub async fn get_closest_peers(
        &self,
        key: &NetworkAddress,
        client: bool,
    ) -> Result<Vec<PeerId>> {
        debug!("Getting the closest peers to {key:?}");
        let (sender, receiver) = oneshot::channel();
        self.send_network_swarm_cmd(NetworkSwarmCmd::GetClosestPeersToAddressFromNetwork {
            key: key.clone(),
            sender,
        });
        let k_bucket_peers = receiver.await?;

        // Count self in if among the CLOSE_GROUP_SIZE closest and sort the result
        let result_len = k_bucket_peers.len();
        let mut closest_peers = k_bucket_peers;
        // ensure we're not including self here
        if client {
            // remove our peer id from the calculations here:
            closest_peers.retain(|&x| x != self.peer_id());
            if result_len != closest_peers.len() {
                info!("Remove self client from the closest_peers");
            }
        }
        if tracing::level_enabled!(tracing::Level::DEBUG) {
            let close_peers_pretty_print: Vec<_> = closest_peers
                .iter()
                .map(|peer_id| {
                    format!(
                        "{peer_id:?}({:?})",
                        PrettyPrintKBucketKey(NetworkAddress::from_peer(*peer_id).as_kbucket_key())
                    )
                })
                .collect();

            debug!("Network knowledge of close peers to {key:?} are: {close_peers_pretty_print:?}");
        }

        let closest_peers = sort_peers_by_address(&closest_peers, key, CLOSE_GROUP_SIZE)?;
        Ok(closest_peers.into_iter().cloned().collect())
    }

    /// Send a `Request` to the provided set of peers and wait for their responses concurrently.
    /// If `get_all_responses` is true, we wait for the responses from all the peers.
    /// NB TODO: Will return an error if the request timeouts.
    /// If `get_all_responses` is false, we return the first successful response that we get
    pub async fn send_and_get_responses(
        &self,
        peers: &[PeerId],
        req: &Request,
        get_all_responses: bool,
    ) -> BTreeMap<PeerId, Result<Response>> {
        debug!("send_and_get_responses for {req:?}");
        let mut list_of_futures = peers
            .iter()
            .map(|peer| {
                Box::pin(async {
                    let resp = self.send_request(req.clone(), *peer).await;
                    (*peer, resp)
                })
            })
            .collect::<Vec<_>>();

        let mut responses = BTreeMap::new();
        while !list_of_futures.is_empty() {
            let ((peer, resp), _, remaining_futures) = select_all(list_of_futures).await;
            let resp_string = match &resp {
                Ok(resp) => format!("{resp}"),
                Err(err) => format!("{err:?}"),
            };
            debug!("Got response from {peer:?} for the req: {req:?}, resp: {resp_string}");
            if !get_all_responses && resp.is_ok() {
                return BTreeMap::from([(peer, resp)]);
            }
            responses.insert(peer, resp);
            list_of_futures = remaining_futures;
        }

        debug!("Received all responses for {req:?}");
        responses
    }
}

/// Given `all_costs` it will return the closest / lowest cost
/// Closest requiring it to be within CLOSE_GROUP nodes
fn get_fees_from_store_cost_responses(
    all_costs: Vec<(NetworkAddress, MainPubkey, PaymentQuote)>,
) -> Result<PayeeQuote> {
    // Find the minimum cost using a linear scan with random tie break
    let mut rng = rand::thread_rng();
    let payee = all_costs
        .into_iter()
        .min_by(
            |(_address_a, _main_key_a, cost_a), (_address_b, _main_key_b, cost_b)| {
                let cmp = cost_a.cost.cmp(&cost_b.cost);
                if cmp == std::cmp::Ordering::Equal {
                    if rng.gen() {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                } else {
                    cmp
                }
            },
        )
        .ok_or(NetworkError::NoStoreCostResponses)?;

    info!("Final fees calculated as: {payee:?}");
    // we dont need to have the address outside of here for now
    let payee_id = if let Some(peer_id) = payee.0.as_peer_id() {
        peer_id
    } else {
        error!("Can't get PeerId from payee {:?}", payee.0);
        return Err(NetworkError::NoStoreCostResponses);
    };
    Ok((payee_id, payee.1, payee.2))
}

/// Get the value of the provided Quorum
pub fn get_quorum_value(quorum: &Quorum) -> usize {
    match quorum {
        Quorum::Majority => close_group_majority(),
        Quorum::All => CLOSE_GROUP_SIZE,
        Quorum::N(v) => v.get(),
        Quorum::One => 1,
    }
}

/// Verifies if `Multiaddr` contains IPv4 address that is not global.
/// This is used to filter out unroutable addresses from the Kademlia routing table.
pub fn multiaddr_is_global(multiaddr: &Multiaddr) -> bool {
    !multiaddr.iter().any(|addr| match addr {
        Protocol::Ip4(ip) => {
            // Based on the nightly `is_global` method (`Ipv4Addrs::is_global`), only using what is available in stable.
            // Missing `is_shared`, `is_benchmarking` and `is_reserved`.
            ip.is_unspecified()
                | ip.is_private()
                | ip.is_loopback()
                | ip.is_link_local()
                | ip.is_documentation()
                | ip.is_broadcast()
        }
        _ => false,
    })
}

/// Pop off the `/p2p/<peer_id>`. This mutates the `Multiaddr` and returns the `PeerId` if it exists.
pub(crate) fn multiaddr_pop_p2p(multiaddr: &mut Multiaddr) -> Option<PeerId> {
    if let Some(Protocol::P2p(peer_id)) = multiaddr.iter().last() {
        // Only actually strip the last protocol if it's indeed the peer ID.
        let _ = multiaddr.pop();
        Some(peer_id)
    } else {
        None
    }
}

/// Build a `Multiaddr` with the p2p protocol filtered out.
/// If it is a relayed address, then the relay's P2P address is preserved.
pub(crate) fn multiaddr_strip_p2p(multiaddr: &Multiaddr) -> Multiaddr {
    let is_relayed = multiaddr.iter().any(|p| matches!(p, Protocol::P2pCircuit));

    if is_relayed {
        // Do not add any PeerId after we've found the P2PCircuit protocol. The prior one is the relay's PeerId which
        // we should preserve.
        let mut before_relay_protocol = true;
        let mut new_multi_addr = Multiaddr::empty();
        for p in multiaddr.iter() {
            if matches!(p, Protocol::P2pCircuit) {
                before_relay_protocol = false;
            }
            if matches!(p, Protocol::P2p(_)) && !before_relay_protocol {
                continue;
            }
            new_multi_addr.push(p);
        }
        new_multi_addr
    } else {
        multiaddr
            .iter()
            .filter(|p| !matches!(p, Protocol::P2p(_)))
            .collect()
    }
}

/// Get the `IpAddr` from the `Multiaddr`
pub(crate) fn multiaddr_get_ip(addr: &Multiaddr) -> Option<IpAddr> {
    addr.iter().find_map(|p| match p {
        Protocol::Ip4(addr) => Some(IpAddr::V4(addr)),
        Protocol::Ip6(addr) => Some(IpAddr::V6(addr)),
        _ => None,
    })
}

pub(crate) fn send_local_swarm_cmd(swarm_cmd_sender: Sender<LocalSwarmCmd>, cmd: LocalSwarmCmd) {
    let capacity = swarm_cmd_sender.capacity();

    if capacity == 0 {
        error!(
            "SwarmCmd channel is full. Await capacity to send: {:?}",
            cmd
        );
    }

    // Spawn a task to send the SwarmCmd and keep this fn sync
    let _handle = spawn(async move {
        if let Err(error) = swarm_cmd_sender.send(cmd).await {
            error!("Failed to send SwarmCmd: {}", error);
        }
    });
}

pub(crate) fn send_network_swarm_cmd(
    swarm_cmd_sender: Sender<NetworkSwarmCmd>,
    cmd: NetworkSwarmCmd,
) {
    let capacity = swarm_cmd_sender.capacity();

    if capacity == 0 {
        error!(
            "SwarmCmd channel is full. Await capacity to send: {:?}",
            cmd
        );
    }

    // Spawn a task to send the SwarmCmd and keep this fn sync
    let _handle = spawn(async move {
        if let Err(error) = swarm_cmd_sender.send(cmd).await {
            error!("Failed to send SwarmCmd: {}", error);
        }
    });
}

#[cfg(test)]
mod tests {
    use eyre::bail;

    use super::*;
    use sn_transfers::PaymentQuote;

    #[test]
    fn test_get_fee_from_store_cost_responses() -> Result<()> {
        // for a vec of different costs of CLOSE_GROUP size
        // ensure we return the CLOSE_GROUP / 2 indexed price
        let mut costs = vec![];
        for i in 1..CLOSE_GROUP_SIZE {
            let addr = MainPubkey::new(bls::SecretKey::random().public_key());
            costs.push((
                NetworkAddress::from_peer(PeerId::random()),
                addr,
                PaymentQuote::test_dummy(Default::default(), NanoTokens::from(i as u64)),
            ));
        }
        let expected_price = costs[0].2.cost.as_nano();
        let (_peer_id, _key, price) = get_fees_from_store_cost_responses(costs)?;

        assert_eq!(
            price.cost.as_nano(),
            expected_price,
            "price should be {expected_price}"
        );

        Ok(())
    }

    #[test]
    fn test_get_some_fee_from_store_cost_responses_even_if_one_errs_and_sufficient(
    ) -> eyre::Result<()> {
        // for a vec of different costs of CLOSE_GROUP size
        let responses_count = CLOSE_GROUP_SIZE as u64 - 1;
        let mut costs = vec![];
        for i in 1..responses_count {
            // push random MainPubkey and Nano
            let addr = MainPubkey::new(bls::SecretKey::random().public_key());
            costs.push((
                NetworkAddress::from_peer(PeerId::random()),
                addr,
                PaymentQuote::test_dummy(Default::default(), NanoTokens::from(i)),
            ));
            println!("price added {i}");
        }

        // this should be the lowest price
        let expected_price = costs[0].2.cost.as_nano();

        let (_peer_id, _key, price) = match get_fees_from_store_cost_responses(costs) {
            Err(_) => bail!("Should not have errored as we have enough responses"),
            Ok(cost) => cost,
        };

        assert_eq!(
            price.cost.as_nano(),
            expected_price,
            "price should be {expected_price}"
        );

        Ok(())
    }

    #[test]
    fn test_network_sign_verify() -> eyre::Result<()> {
        let (network, _, _) =
            NetworkBuilder::new(Keypair::generate_ed25519(), false, std::env::temp_dir())
                .build_client()?;
        let msg = b"test message";
        let sig = network.sign(msg)?;
        assert!(network.verify(msg, &sig));
        Ok(())
    }
}
