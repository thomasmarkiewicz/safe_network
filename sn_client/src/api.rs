// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    chunks::Error as ChunksError,
    error::{Error, Result},
    Client, ClientEvent, ClientEventsChannel, ClientEventsReceiver, ClientRegister, WalletClient,
};
use bls::{PublicKey, SecretKey, Signature};
use bytes::Bytes;
use futures::future::join_all;
use indicatif::ProgressBar;
use libp2p::{
    identity::Keypair,
    kad::{Quorum, Record},
    Multiaddr, PeerId,
};
#[cfg(feature = "open-metrics")]
use prometheus_client::registry::Registry;
use rand::{thread_rng, Rng};
use sn_networking::{
    multiaddr_is_global, Error as NetworkError, GetRecordCfg, GetRecordError, NetworkBuilder,
    NetworkEvent, PutRecordCfg, VerificationKind, CLOSE_GROUP_SIZE,
};
use sn_protocol::{
    error::Error as ProtocolError,
    messages::ChunkProof,
    storage::{
        try_deserialize_record, try_serialize_record, Chunk, ChunkAddress, RecordHeader,
        RecordKind, RegisterAddress, SpendAddress,
    },
    NetworkAddress, PrettyPrintRecordKey,
};
use sn_registers::SignedRegister;
use sn_transfers::{CashNote, CashNoteRedemption, MainPubkey, NanoTokens, Payment, SignedSpend};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::PathBuf,
    time::Duration,
};
use tokio::task::spawn;
use tracing::trace;
use xor_name::XorName;

/// The maximum duration the client will wait for a connection to the network before timing out.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(180);

/// The timeout duration for the client to receive any response from the network.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

impl Client {
    /// Instantiate a new client.
    ///
    /// Optionally specify the maximum time the client will wait for a connection to the network before timing out.
    /// Defaults to 180s
    pub async fn new(
        signer: SecretKey,
        peers: Option<Vec<Multiaddr>>,
        enable_gossip: bool,
        connection_timeout: Option<Duration>,
    ) -> Result<Self> {
        // If any of our contact peers has a global address, we'll assume we're in a global network.
        let local = match peers {
            Some(ref peers) => !peers.iter().any(multiaddr_is_global),
            None => true,
        };

        info!("Startup a client with peers {peers:?} and local {local:?} flag");
        info!("Starting Kad swarm in client mode...");

        let mut network_builder =
            NetworkBuilder::new(Keypair::generate_ed25519(), local, std::env::temp_dir());

        if enable_gossip {
            network_builder.enable_gossip();
        }

        #[cfg(feature = "open-metrics")]
        network_builder.metrics_registry(Registry::default());

        let (network, mut network_event_receiver, swarm_driver) = network_builder.build_client()?;
        info!("Client constructed network and swarm_driver");
        let events_channel = ClientEventsChannel::default();

        let client = Self {
            network: network.clone(),
            events_channel,
            signer,
            peers_added: 0,
            progress: Some(Self::setup_connection_progress()),
        };

        // subscribe to our events channel first, so we don't have intermittent
        // errors if it does not exist and we cannot send to it.
        // (eg, if PeerAdded happens faster than our events channel is created)
        let mut client_events_rx = client.events_channel();

        let _swarm_driver = spawn({
            trace!("Starting up client swarm_driver");
            swarm_driver.run()
        });

        // spawn task to dial to the given peers
        let network_clone = network.clone();
        let _handle = spawn(async move {
            if let Some(peers) = peers {
                for addr in peers {
                    trace!(%addr, "dialing initial peer");

                    if let Err(err) = network_clone.dial(addr.clone()).await {
                        tracing::error!(%addr, "Failed to dial: {err:?}");
                    };
                }
            }
        });

        // spawn task to wait for NetworkEvent and check for inactivity
        let mut client_clone = client.clone();
        let _event_handler = spawn(async move {
            loop {
                match tokio::time::timeout(INACTIVITY_TIMEOUT, network_event_receiver.recv()).await
                {
                    Ok(event) => {
                        let the_event = match event {
                            Some(the_event) => the_event,
                            None => {
                                error!("The `NetworkEvent` channel has been closed");
                                continue;
                            }
                        };

                        let start = std::time::Instant::now();
                        let event_string = format!("{the_event:?}");
                        if let Err(err) = client_clone.handle_network_event(the_event) {
                            warn!("Error handling network event: {err}");
                        }
                        trace!(
                            "Handled network event in {:?}: {:?}",
                            start.elapsed(),
                            event_string
                        );
                    }
                    Err(_elapse_err) => {
                        debug!("Client inactivity... waiting for a network event");
                        if let Err(error) = client_clone
                            .events_channel
                            .broadcast(ClientEvent::InactiveClient(INACTIVITY_TIMEOUT))
                        {
                            error!("Error broadcasting inactive client event: {error}");
                        }
                    }
                }
            }
        });

        // loop to connect to the network
        let mut is_connected = false;
        let connection_timeout = connection_timeout.unwrap_or(CONNECTION_TIMEOUT);
        let mut connection_timeout_interval = tokio::time::interval(connection_timeout);
        // first tick completes immediately
        connection_timeout_interval.tick().await;

        loop {
            tokio::select! {
            _ = connection_timeout_interval.tick() => {
                if !is_connected {
                    error!("Timeout: Client failed to connect to the network within {connection_timeout:?}");
                    return Err(Error::ConnectionTimeout(connection_timeout));
                }
            }
            event = client_events_rx.recv() => {
                match event {
                    Ok(ClientEvent::ConnectedToNetwork) => {
                        is_connected = true;
                        info!("Client connected to the Network {is_connected:?}.");
                        break;
                    }
                    Ok(ClientEvent::InactiveClient(timeout)) => {
                        if is_connected {
                            info!("The client was inactive for {timeout:?}.");
                        } else {
                            info!("The client still does not know enough network nodes.");
                        }
                        continue;
                    }
                    Ok(ClientEvent::GossipsubMsg { .. }) => {}
                    Err(err) => {
                        error!("Unexpected error during client startup {err:?}");
                        println!("Unexpected error during client startup {err:?}");
                        return Err(err);
                    }
                }
            }}
        }

        // The above loop breaks if `ConnectedToNetwork` is received, but we might need the
        // receiver to still be active for us to not get any error if any other event is sent
        let mut client_events_rx = client.events_channel();
        spawn(async move {
            loop {
                let _ = client_events_rx.recv().await;
            }
        });
        Ok(client)
    }

    /// Set up our initial progress bar for network connectivity
    fn setup_connection_progress() -> ProgressBar {
        // Network connection progress bar
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(Duration::from_millis(120));
        progress.set_message("Connecting to The SAFE Network...");
        let new_style = progress.style().tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈🔗");
        progress.set_style(new_style);

        progress.set_message("Connecting to The SAFE Network...");

        progress
    }

    fn handle_network_event(&mut self, event: NetworkEvent) -> Result<()> {
        match event {
            NetworkEvent::PeerAdded(peer_id, _connected_peer) => {
                self.peers_added += 1;
                debug!("PeerAdded: {peer_id}");

                // In case client running in non-local-discovery mode,
                // it may take some time to fill up the RT.
                // To avoid such delay may fail the query with RecordNotFound,
                // wait till certain amount of peers populated into RT
                if self.peers_added >= CLOSE_GROUP_SIZE {
                    if let Some(progress) = &self.progress {
                        progress.finish_with_message("Connected to the Network");
                        // Remove the progress bar
                        self.progress = None;
                    }

                    self.events_channel
                        .broadcast(ClientEvent::ConnectedToNetwork)?;
                } else {
                    debug!(
                        "{}/{CLOSE_GROUP_SIZE} initial peers found.",
                        self.peers_added
                    );

                    if let Some(progress) = &self.progress {
                        progress.set_message(format!(
                            "{}/{CLOSE_GROUP_SIZE} initial peers found.",
                            self.peers_added
                        ));
                    }
                }
            }
            NetworkEvent::GossipsubMsgReceived { topic, msg }
            | NetworkEvent::GossipsubMsgPublished { topic, msg } => {
                self.events_channel
                    .broadcast(ClientEvent::GossipsubMsg { topic, msg })?;
            }
            _other => {}
        }

        Ok(())
    }

    /// Get the client events channel.
    pub fn events_channel(&self) -> ClientEventsReceiver {
        self.events_channel.subscribe()
    }

    /// Sign the given data
    pub fn sign<T: AsRef<[u8]>>(&self, data: T) -> Signature {
        self.signer.sign(data)
    }

    /// Return a reference to the signer secret key
    pub fn signer(&self) -> &SecretKey {
        &self.signer
    }

    /// Return the public key of the data signing key
    pub fn signer_pk(&self) -> PublicKey {
        self.signer.public_key()
    }

    /// Get a register from network
    pub async fn get_signed_register_from_network(
        &self,
        address: RegisterAddress,
        is_verifying: bool,
    ) -> Result<SignedRegister> {
        let key = NetworkAddress::from_register_address(address).to_record_key();
        let quorum = if is_verifying {
            Quorum::N(NonZeroUsize::new(2).ok_or(Error::NonZeroUsizeWasInitialisedAsZero)?)
        } else {
            Quorum::One
        };
        let get_cfg = GetRecordCfg {
            get_quorum: quorum,
            re_attempt: true,
            target_record: None,
            expected_holders: Default::default(),
        };

        let maybe_record = self.network.get_record_from_network(key, &get_cfg).await;
        let record = match &maybe_record {
            Ok(r) => r,
            Err(NetworkError::GetRecordError(GetRecordError::SplitRecord { result_map })) => {
                return merge_split_register_records(address, result_map)
            }
            Err(e) => {
                warn!("Failed to get record at {address:?} from the network: {e:?}");
                return Err(ProtocolError::RegisterNotFound(Box::new(address)).into());
            }
        };

        debug!(
            "Got record from the network, {:?}",
            PrettyPrintRecordKey::from(&record.key)
        );

        let register = get_register_from_record(record)
            .map_err(|_| ProtocolError::RegisterNotFound(Box::new(address)))?;
        Ok(register)
    }

    /// Retrieve a Register from the network.
    pub async fn get_register(&self, address: RegisterAddress) -> Result<ClientRegister> {
        info!("Retrieving a Register replica at {address}");
        ClientRegister::retrieve(self.clone(), address).await
    }

    /// Create a new Register on the Network.
    /// Tops up payments and retries if necessary and verification failed
    pub async fn create_and_pay_for_register(
        &self,
        address: XorName,
        wallet_client: &mut WalletClient,
        verify_store: bool,
    ) -> Result<(ClientRegister, NanoTokens, NanoTokens)> {
        info!("Instantiating a new Register replica with address {address:?}");
        let (reg, mut total_cost, mut total_royalties) =
            ClientRegister::create_online(self.clone(), address, wallet_client, false).await?;

        debug!("{address:?} Created in theorryyyyy");
        let reg_address = reg.address();
        if verify_store {
            debug!("WE SHOULD VERRRRIFYING");
            let mut stored = self.verify_register_stored(*reg_address).await.is_ok();

            while !stored {
                info!("Register not completely stored on the network yet. Retrying...");
                // this verify store call here ensures we get the record from Quorum::all
                let (reg, top_up_cost, royalties_top_up) =
                    ClientRegister::create_online(self.clone(), address, wallet_client, true)
                        .await?;
                let reg_address = reg.address();

                total_cost = total_cost
                    .checked_add(top_up_cost)
                    .ok_or(Error::TotalPriceTooHigh)?;
                total_royalties =
                    total_cost
                        .checked_add(royalties_top_up)
                        .ok_or(Error::Transfers(sn_transfers::WalletError::from(
                            sn_transfers::Error::ExcessiveNanoValue,
                        )))?;
                stored = self.verify_register_stored(*reg_address).await.is_ok();
            }
        }

        Ok((reg, total_cost, total_royalties))
    }

    /// Store `Chunk` as a record.
    pub(super) async fn store_chunk(
        &self,
        chunk: Chunk,
        payee: PeerId,
        payment: Payment,
        verify_store: bool,
    ) -> Result<()> {
        info!("Store chunk: {:?}", chunk.address());
        let key = chunk.network_address().to_record_key();

        let record_kind = RecordKind::ChunkWithPayment;
        let record = Record {
            key: key.clone(),
            value: try_serialize_record(&(payment, chunk.clone()), record_kind)?.to_vec(),
            publisher: None,
            expires: None,
        };

        let verification = if verify_store {
            let verification_cfg = GetRecordCfg {
                get_quorum: Quorum::N(
                    NonZeroUsize::new(2).ok_or(Error::NonZeroUsizeWasInitialisedAsZero)?,
                ),
                re_attempt: true,
                target_record: None, // Not used since we use ChunkProof
                expected_holders: Default::default(),
            };
            // The `ChunkWithPayment` is only used to send out via PutRecord.
            // The holders shall only hold the `Chunk` copies.
            // Hence the fetched copies shall only be a `Chunk`

            let stored_on_node = try_serialize_record(&chunk, RecordKind::Chunk)?.to_vec();
            let random_nonce = thread_rng().gen::<u64>();
            let expected_proof = ChunkProof::new(&stored_on_node, random_nonce);

            Some((
                VerificationKind::ChunkProof {
                    expected_proof,
                    nonce: random_nonce,
                },
                verification_cfg,
            ))
        } else {
            None
        };
        let put_cfg = PutRecordCfg {
            put_quorum: Quorum::One,
            re_attempt: true,
            use_put_record_to: Some(vec![payee]),
            verification,
        };
        Ok(self.network.put_record(record, &put_cfg).await?)
    }

    /// Retrieve a `Chunk` from the kad network.
    pub async fn get_chunk(&self, address: ChunkAddress, show_holders: bool) -> Result<Chunk> {
        info!("Getting chunk: {address:?}");
        let key = NetworkAddress::from_chunk_address(address).to_record_key();

        let expected_holders = if show_holders {
            let result: HashSet<_> = self
                .network
                .get_closest_peers(&NetworkAddress::from_chunk_address(address), true)
                .await?
                .iter()
                .cloned()
                .collect();
            result
        } else {
            Default::default()
        };

        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::One,
            re_attempt: true,
            target_record: None,
            expected_holders,
        };
        let record = self.network.get_record_from_network(key, &get_cfg).await?;
        let header = RecordHeader::from_record(&record)?;
        if let RecordKind::Chunk = header.kind {
            let chunk: Chunk = try_deserialize_record(&record)?;
            Ok(chunk)
        } else {
            Err(NetworkError::RecordKindMismatch(RecordKind::Chunk).into())
        }
    }

    /// Verify if a `Chunk` is stored by expected nodes on the network.
    pub async fn verify_chunk_stored(&self, chunk: &Chunk) -> Result<()> {
        let address = chunk.network_address();
        info!("Verifying chunk: {address:?}");
        let random_nonce = thread_rng().gen::<u64>();
        let record_value = try_serialize_record(&chunk, RecordKind::Chunk)?;
        let expected_proof = ChunkProof::new(record_value.as_ref(), random_nonce);

        if let Err(err) = self
            .network
            .verify_chunk_existence(
                address.clone(),
                random_nonce,
                expected_proof,
                Quorum::N(NonZeroUsize::new(2).ok_or(Error::NonZeroUsizeWasInitialisedAsZero)?),
                false,
            )
            .await
        {
            error!("Failed to verify the existence of chunk {address:?} with err {err:?}");
        }

        Ok(())
    }

    /// Verify if a `Register` is stored by expected nodes on the network.
    pub async fn verify_register_stored(&self, address: RegisterAddress) -> Result<SignedRegister> {
        info!("Verifying register: {address:?}");
        self.get_signed_register_from_network(address, true).await
    }

    /// Send a `SpendCashNote` request to the network
    pub(crate) async fn network_store_spend(
        &self,
        spend: SignedSpend,
        verify_store: bool,
    ) -> Result<()> {
        let unique_pubkey = *spend.unique_pubkey();
        let cash_note_addr = SpendAddress::from_unique_pubkey(&unique_pubkey);
        let network_address = NetworkAddress::from_spend_address(cash_note_addr);

        trace!("Sending spend {unique_pubkey:?} to the network via put_record, with addr of {cash_note_addr:?}");
        let key = network_address.to_record_key();
        let record_kind = RecordKind::Spend;
        let record = Record {
            key,
            value: try_serialize_record(&[spend], record_kind)?.to_vec(),
            publisher: None,
            expires: None,
        };

        let (record_to_verify, expected_holders) = if verify_store {
            let expected_holders: HashSet<_> = self
                .network
                .get_closest_peers(&network_address, true)
                .await?
                .iter()
                .cloned()
                .collect();
            (Some(record.clone()), expected_holders)
        } else {
            (None, Default::default())
        };

        let verification_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            re_attempt: true,
            target_record: record_to_verify,
            expected_holders,
        };
        let put_cfg = PutRecordCfg {
            put_quorum: Quorum::All,
            re_attempt: true,
            use_put_record_to: None,
            verification: Some((VerificationKind::Network, verification_cfg)),
        };
        Ok(self.network.put_record(record, &put_cfg).await?)
    }

    /// Get a spend from network
    pub async fn get_spend_from_network(&self, address: SpendAddress) -> Result<SignedSpend> {
        let key = NetworkAddress::from_spend_address(address).to_record_key();

        trace!(
            "Getting spend at {address:?} with record_key {:?}",
            PrettyPrintRecordKey::from(&key)
        );
        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            re_attempt: true,
            target_record: None,
            expected_holders: Default::default(),
        };
        let record = self
            .network
            .get_record_from_network(key.clone(), &get_cfg)
            .await
            .map_err(|err| match err {
                sn_networking::Error::GetRecordError(GetRecordError::RecordNotFound) => {
                    Error::MissingSpendRecord(address)
                }
                _ => Error::CouldNotVerifyTransfer(format!(
                    "failed to get spend at {address:?}: {err:?}"
                )),
            })?;
        debug!(
            "For spend at {address:?} got record from the network, {:?}",
            PrettyPrintRecordKey::from(&record.key)
        );

        let header = RecordHeader::from_record(&record).map_err(|err| {
            Error::CouldNotVerifyTransfer(format!(
                "Can't parse RecordHeader for the spend at {address:?} with error {err:?}"
            ))
        })?;

        if let RecordKind::Spend = header.kind {
            let mut deserialized_record = try_deserialize_record::<Vec<SignedSpend>>(&record)
                .map_err(|err| {
                    Error::CouldNotVerifyTransfer(format!(
                        "Can't deserialize record for the spend at {address:?} with error {err:?}"
                    ))
                })?;

            match deserialized_record.len() {
                0 => {
                    trace!("Found no spend for {address:?}");
                    Err(Error::CouldNotVerifyTransfer(format!(
                        "Fetched record shows no spend for cash_note {address:?}."
                    )))
                }
                1 => {
                    let signed_spend = deserialized_record.remove(0);
                    trace!("Spend get for address: {address:?} successful");
                    if address == SpendAddress::from_unique_pubkey(signed_spend.unique_pubkey()) {
                        match signed_spend.verify(signed_spend.spent_tx_hash()) {
                            Ok(_) => {
                                trace!("Verified signed spend got from network for {address:?}");
                                Ok(signed_spend)
                            }
                            Err(err) => {
                                warn!("Invalid signed spend got from network for {address:?}: {err:?}.");
                                Err(Error::CouldNotVerifyTransfer(format!(
                                "Spend failed verifiation for the unique_pubkey {address:?} with error {err:?}")))
                            }
                        }
                    } else {
                        warn!("Signed spend ({:?}) got from network mismatched the expected one {address:?}.", signed_spend.unique_pubkey());
                        Err(Error::CouldNotVerifyTransfer(format!(
                                "Signed spend ({:?}) got from network mismatched the expected one {address:?}.", signed_spend.unique_pubkey())))
                    }
                }
                _ => {
                    // each one is 0 as it shifts remaining elements
                    let one = deserialized_record.remove(0);
                    let two = deserialized_record.remove(0);
                    error!("Found double spend for {address:?}");
                    Err(Error::CouldNotVerifyTransfer(format!(
                "Found double spend for the unique_pubkey {address:?} - {:?}: spend_one {:?} and spend_two {:?}",
                PrettyPrintRecordKey::from(&key), one.derived_key_sig, two.derived_key_sig
            )))
                }
            }
        } else {
            error!("RecordKind mismatch while trying to retrieve a cash_note spend");
            Err(NetworkError::RecordKindMismatch(RecordKind::Spend).into())
        }
    }

    /// Subscribe to given gossipsub topic
    pub fn subscribe_to_topic(&self, topic_id: String) -> Result<()> {
        info!("Subscribing to topic id: {topic_id}");
        self.network.subscribe_to_topic(topic_id)?;
        self.network.start_handle_gossip()?;
        Ok(())
    }

    /// Unsubscribe from given gossipsub topic
    pub fn unsubscribe_from_topic(&self, topic_id: String) -> Result<()> {
        info!("Unsubscribing from topic id: {topic_id}");
        self.network.unsubscribe_from_topic(topic_id)?;
        Ok(())
    }

    /// Publish message on given topic
    pub fn publish_on_topic(&self, topic_id: String, msg: Bytes) -> Result<()> {
        info!("Publishing msg on topic id: {topic_id}");
        self.network.publish_on_topic(topic_id, msg)?;
        Ok(())
    }

    /// This function is used to receive a list of CashNoteRedemptions and turn it back into spendable CashNotes.
    /// Needs Network connection.
    /// Verify CashNoteRedemptions and rebuild spendable currency from them.
    /// Returns an `Error::InvalidTransfer` if any CashNoteRedemption is not valid
    /// Else returns a list of CashNotes that can be spent by the owner.
    pub async fn verify_cash_notes_redemptions(
        &self,
        main_pubkey: MainPubkey,
        cashnote_redemptions: &[CashNoteRedemption],
    ) -> Result<Vec<CashNote>> {
        let cash_notes = self
            .network
            .verify_cash_notes_redemptions(main_pubkey, cashnote_redemptions)
            .await?;
        Ok(cash_notes)
    }

    /// Verify that chunks were uploaded
    ///
    /// Returns a vec of any chunks that could not be verified
    pub async fn verify_uploaded_chunks(
        &self,
        chunks_paths: &[(XorName, PathBuf)],
        batch_size: usize,
    ) -> Result<Vec<(XorName, PathBuf)>> {
        let mut failed_chunks = Vec::new();

        for chunks_batch in chunks_paths.chunks(batch_size) {
            // now we try and get batched chunks, keep track of any that fail
            // Iterate over each uploaded chunk
            let mut verify_handles = Vec::new();
            for (name, chunk_path) in chunks_batch.iter().cloned() {
                let client = self.clone();
                // Spawn a new task to fetch each chunk concurrently
                let handle = tokio::spawn(async move {
                    // make sure the chunk is stored;
                    let chunk = Chunk::new(Bytes::from(std::fs::read(&chunk_path)?));
                    let res = client.verify_chunk_stored(&chunk).await;

                    Ok::<_, ChunksError>(((name, chunk_path), res.is_err()))
                });
                verify_handles.push(handle);
            }

            // Await all fetch tasks and collect the results
            let verify_results = join_all(verify_handles).await;

            // Check for any errors during fetch
            for result in verify_results {
                if let ((chunk_addr, path), true) = result?? {
                    warn!("Failed to fetch a chunk {chunk_addr:?}");
                    failed_chunks.push((chunk_addr, path));
                }
            }
        }

        Ok(failed_chunks)
    }
}

fn get_register_from_record(record: &Record) -> Result<SignedRegister> {
    let header = RecordHeader::from_record(record)?;

    if let RecordKind::Register = header.kind {
        let register = try_deserialize_record::<SignedRegister>(record)?;
        Ok(register)
    } else {
        error!("RecordKind mismatch while trying to retrieve a signed register");
        Err(NetworkError::RecordKindMismatch(RecordKind::Register).into())
    }
}

/// if multiple register records where found for a given key, merge them into a single register
fn merge_split_register_records(
    address: RegisterAddress,
    map: &HashMap<XorName, (Record, HashSet<PeerId>)>,
) -> Result<SignedRegister> {
    let key = NetworkAddress::from_register_address(address).to_record_key();
    let pretty_key = PrettyPrintRecordKey::from(&key);
    debug!("Got multiple records from the network for key: {pretty_key:?}");
    let mut all_registers = vec![];
    for (record, peers) in map.values() {
        match get_register_from_record(record) {
            Ok(r) => all_registers.push(r),
            Err(e) => {
                warn!("Ignoring invalid register record found for {pretty_key:?} received from {peers:?}: {:?}", e);
                continue;
            }
        }
    }

    // get the first valid register
    let one_valid_reg = if let Some(r) = all_registers.clone().iter().find(|r| r.verify().is_ok()) {
        r.clone()
    } else {
        error!("No valid register records found for {key:?}");
        return Err(Error::Protocol(ProtocolError::RegisterNotFound(Box::new(
            address,
        ))));
    };

    // merge it with the others if they are valid
    let register: SignedRegister = all_registers.into_iter().fold(one_valid_reg, |mut acc, r| {
        if acc.verified_merge(r).is_err() {
            warn!("Skipping register that failed to merge. Entry found for {key:?}");
        }
        acc
    });

    Ok(register)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sn_registers::Register;

    use super::*;

    #[test]
    fn test_merge_split_register_records() -> eyre::Result<()> {
        let mut rng = rand::thread_rng();
        let meta = XorName::random(&mut rng);
        let owner_sk = SecretKey::random();
        let owner_pk = owner_sk.public_key();
        let address = RegisterAddress::new(meta, owner_pk);
        let peers1 = HashSet::from_iter(vec![PeerId::random(), PeerId::random()]);
        let peers2 = HashSet::from_iter(vec![PeerId::random(), PeerId::random()]);

        // prepare registers
        let mut register_root = Register::new(owner_pk, meta, Default::default());
        let (root_hash, _) =
            register_root.write(b"root_entry".to_vec(), &BTreeSet::default(), &owner_sk)?;
        let root = BTreeSet::from_iter(vec![root_hash]);
        let signed_root = register_root.clone().into_signed(&owner_sk)?;

        let mut register1 = register_root.clone();
        let (_hash, op1) = register1.write(b"entry1".to_vec(), &root, &owner_sk)?;
        let mut signed_register1 = signed_root.clone();
        signed_register1.add_op(op1)?;

        let mut register2 = register_root.clone();
        let (_hash, op2) = register2.write(b"entry2".to_vec(), &root, &owner_sk)?;
        let mut signed_register2 = signed_root;
        signed_register2.add_op(op2)?;

        let mut register_bad = Register::new(owner_pk, meta, Default::default());
        let (_hash, _op_bad) =
            register_bad.write(b"bad_root".to_vec(), &BTreeSet::default(), &owner_sk)?;
        let invalid_sig = register2.sign(&owner_sk)?; // steal sig from something else
        let signed_register_bad = SignedRegister::new(register_bad, invalid_sig);

        // prepare records
        let record1 = Record {
            key: NetworkAddress::from_register_address(address).to_record_key(),
            value: try_serialize_record(&signed_register1, RecordKind::Register)?.to_vec(),
            publisher: None,
            expires: None,
        };
        let xorname1 = XorName::from_content(&record1.value);
        let record2 = Record {
            key: NetworkAddress::from_register_address(address).to_record_key(),
            value: try_serialize_record(&signed_register2, RecordKind::Register)?.to_vec(),
            publisher: None,
            expires: None,
        };
        let xorname2 = XorName::from_content(&record2.value);
        let record_bad = Record {
            key: NetworkAddress::from_register_address(address).to_record_key(),
            value: try_serialize_record(&signed_register_bad, RecordKind::Register)?.to_vec(),
            publisher: None,
            expires: None,
        };
        let xorname_bad = XorName::from_content(&record_bad.value);

        // test with 2 valid records: should return the two merged
        let mut expected_merge = signed_register1.clone();
        expected_merge.merge(signed_register2)?;
        let map = HashMap::from_iter(vec![
            (xorname1, (record1.clone(), peers1.clone())),
            (xorname2, (record2, peers2.clone())),
        ]);
        let reg = merge_split_register_records(address, &map)?; // Ok
        assert_eq!(reg, expected_merge);

        // test with 1 valid record and 1 invalid record: should return the valid one
        let map = HashMap::from_iter(vec![
            (xorname1, (record1, peers1.clone())),
            (xorname2, (record_bad.clone(), peers2.clone())),
        ]);
        let reg = merge_split_register_records(address, &map)?; // Ok
        assert_eq!(reg, signed_register1);

        // test with 2 invalid records: should error out
        let map = HashMap::from_iter(vec![
            (xorname_bad, (record_bad.clone(), peers1)),
            (xorname_bad, (record_bad, peers2)),
        ]);
        let res = merge_split_register_records(address, &map); // Err
        assert!(res.is_err());

        Ok(())
    }
}
