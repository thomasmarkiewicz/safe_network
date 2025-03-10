// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#![allow(clippy::mutable_key_type)]
mod common;

use crate::common::{
    client::{
        get_all_rpc_addresses, get_gossip_client, get_gossip_client_and_wallet,
        PAYING_WALLET_INITIAL_BALANCE,
    },
    get_all_peer_ids, node_restart,
};
use assert_fs::TempDir;
use eyre::{eyre, Result};
use libp2p::{
    kad::{KBucketKey, RecordKey},
    PeerId,
};
use rand::{rngs::OsRng, Rng};
use sn_client::{Client, FilesApi, FilesUpload};
use sn_logging::LogBuilder;
use sn_networking::{sort_peers_by_key, CLOSE_GROUP_SIZE};
use sn_protocol::{
    safenode_proto::{safe_node_client::SafeNodeClient, NodeInfoRequest, RecordAddressesRequest},
    storage::ChunkAddress,
    NetworkAddress, PrettyPrintRecordKey,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::File,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};
use tonic::Request;
use tracing::error;
use xor_name::XorName;

const CHUNK_SIZE: usize = 1024;

// VERIFICATION_DELAY is set based on the dead peer detection interval
// Once a node has been restarted, it takes VERIFICATION_DELAY time
// for the old peer to be removed from the routing table.
// Replication is then kicked off to distribute the data to the new closest
// nodes, hence verification has to be performed after this.
const VERIFICATION_DELAY: Duration = Duration::from_secs(20);

/// Number of times to retry verification if it fails
const VERIFICATION_ATTEMPTS: usize = 5;

/// Length of time to wait before re-verifying the data location
const REVERIFICATION_DELAY: Duration =
    Duration::from_secs(sn_node::PERIODIC_REPLICATION_INTERVAL_MAX_S);

// Default number of churns that should be performed. After each churn, we
// wait for VERIFICATION_DELAY time before verifying the data location.
// It can be overridden by setting the 'CHURN_COUNT' env var.
const CHURN_COUNT: u8 = 20;

/// Default number of chunks that should be PUT to the network.
// It can be overridden by setting the 'CHUNK_COUNT' env var.
const CHUNK_COUNT: usize = 50;

type NodeIndex = usize;
type RecordHolders = HashMap<RecordKey, HashSet<NodeIndex>>;

#[tokio::test(flavor = "multi_thread")]
async fn verify_data_location() -> Result<()> {
    let _log_appender_guard = LogBuilder::init_multi_threaded_tokio_test("verify_data_location");

    query_content_task();

    let churn_count = if let Ok(str) = std::env::var("CHURN_COUNT") {
        str.parse::<u8>()?
    } else {
        CHURN_COUNT
    };
    let chunk_count = if let Ok(str) = std::env::var("CHUNK_COUNT") {
        str.parse::<usize>()?
    } else {
        CHUNK_COUNT
    };
    println!(
        "Performing data location verification with a churn count of {churn_count} and n_chunks {chunk_count}\nIt will take approx {:?}",
        VERIFICATION_DELAY*churn_count as u32
    );
    let node_rpc_address = get_all_rpc_addresses()?;
    let mut all_peers = get_all_peer_ids(&node_rpc_address).await?;

    // Store chunks
    println!("Creating a client and paying wallet...");
    let paying_wallet_dir = TempDir::new()?;

    let (client, _paying_wallet) =
        get_gossip_client_and_wallet(paying_wallet_dir.path(), PAYING_WALLET_INITIAL_BALANCE)
            .await?;

    store_chunks(client, chunk_count, paying_wallet_dir.to_path_buf()).await?;

    // Verify data location initially
    verify_location(&all_peers, &node_rpc_address).await?;

    // Churn nodes and verify the location of the data after VERIFICATION_DELAY
    let mut current_churn_count = 0;

    'main: loop {
        for (node_index, rpc_address) in node_rpc_address.iter().enumerate() {
            if current_churn_count >= churn_count {
                break 'main Ok(());
            }
            current_churn_count += 1;

            // restart a node
            node_restart(rpc_address).await?;

            // wait for the dead peer to be removed from the RT and the replication flow to finish
            println!("\nNode {node_index} has been restarted, waiting for {VERIFICATION_DELAY:?} before verification");
            tokio::time::sleep(VERIFICATION_DELAY).await;

            // get the new PeerId for the current NodeIndex
            let endpoint = format!("https://{rpc_address}");
            let mut rpc_client = SafeNodeClient::connect(endpoint).await?;
            let response = rpc_client
                .node_info(Request::new(NodeInfoRequest {}))
                .await?;
            let peer_id = PeerId::from_bytes(&response.get_ref().peer_id)?;
            all_peers[node_index] = peer_id;

            print_node_close_groups(&all_peers);

            verify_location(&all_peers, &node_rpc_address).await?;
        }
    }
}

// Spawns a task which periodically queries random content
fn query_content_task() {
    let _handle = tokio::spawn(async move {
        let delay = Duration::from_millis(1);
        loop {
            // thread random rng
            let mut rng = OsRng;
            let client = get_gossip_client().await;
            // let's choose a random content to query
            let rando_addr = ChunkAddress::new(XorName::random(&mut rng));
            tracing::trace!("Querying content {rando_addr:?}");

            if let Err(error) = client.get_chunk(rando_addr, false).await {
                tracing::error!(
                    "Error querying content rando content (as expected) {rando_addr:?} : {error}"
                );
            }

            tokio::time::sleep(delay).await;
        }
    });
}

fn print_node_close_groups(all_peers: &[PeerId]) {
    let all_peers = all_peers.to_vec();
    println!("\nNode close groups:");

    for (node_index, peer) in all_peers.iter().enumerate() {
        let key = NetworkAddress::from_peer(*peer).as_kbucket_key();
        let closest_peers =
            sort_peers_by_key(&all_peers, &key, CLOSE_GROUP_SIZE).expect("failed to sort peer");
        let closest_peers_idx = closest_peers
            .iter()
            .map(|&&peer| {
                all_peers
                    .iter()
                    .position(|&p| p == peer)
                    .expect("peer to be in iterator")
            })
            .collect::<Vec<_>>();
        println!("Close for {node_index}: {peer:?} are {closest_peers_idx:?}");
    }
}

async fn get_records_and_holders(node_rpc_addresses: &[SocketAddr]) -> Result<RecordHolders> {
    let mut record_holders = RecordHolders::default();

    for (node_index, rpc_address) in node_rpc_addresses.iter().enumerate() {
        let endpoint = format!("https://{rpc_address}");
        let mut rpc_client = SafeNodeClient::connect(endpoint).await?;

        let records_response = rpc_client
            .record_addresses(Request::new(RecordAddressesRequest {}))
            .await?;

        for bytes in records_response.get_ref().addresses.iter() {
            let key = RecordKey::from(bytes.clone());
            let holders = record_holders.entry(key).or_insert(HashSet::new());
            holders.insert(node_index);
        }
    }
    println!("Obtained the current set of Record Key holders");
    Ok(record_holders)
}

// Fetches the record_holders and verifies that the record is stored by the actual closest peers to the RecordKey
// It has a retry loop built in.
async fn verify_location(all_peers: &Vec<PeerId>, node_rpc_addresses: &[SocketAddr]) -> Result<()> {
    let mut failed = HashMap::new();

    println!("*********************************************");
    println!("Verifying data across all peers {all_peers:?}");
    tracing::info!("*********************************************");
    tracing::info!("Verifying data across all peers {all_peers:?}");

    let mut verification_attempts = 0;
    while verification_attempts < VERIFICATION_ATTEMPTS {
        failed.clear();
        let record_holders = get_records_and_holders(node_rpc_addresses).await?;
        for (key, actual_holders_idx) in record_holders.iter() {
            println!("Verifying {:?}", PrettyPrintRecordKey::from(key));
            let record_key = KBucketKey::from(key.to_vec());
            let expected_holders = sort_peers_by_key(all_peers, &record_key, CLOSE_GROUP_SIZE)?
                .into_iter()
                .cloned()
                .collect::<BTreeSet<_>>();

            let actual_holders = actual_holders_idx
                .iter()
                .map(|i| all_peers[*i])
                .collect::<BTreeSet<_>>();

            println!(
                "Expected to be held by {:?} nodes: {expected_holders:?}",
                expected_holders.len()
            );
            println!(
                "Actually held by {:?} nodes      : {actual_holders:?}",
                actual_holders.len()
            );

            if actual_holders != expected_holders {
                // print any expect holders that are not in actual holders
                let mut missing_peers = Vec::new();
                expected_holders
                    .iter()
                    .filter(|expected| !actual_holders.contains(expected))
                    .for_each(|expected| missing_peers.push(*expected));

                error!(
                    "Record {:?} is not stored by {missing_peers:?}",
                    PrettyPrintRecordKey::from(key),
                );
                println!(
                    "Record {:?} is not stored by {missing_peers:?}",
                    PrettyPrintRecordKey::from(key),
                );
            }

            let mut failed_peers = Vec::new();
            expected_holders
                .iter()
                .filter(|expected| !actual_holders.contains(expected))
                .for_each(|expected| failed_peers.push(*expected));

            if !failed_peers.is_empty() {
                failed.insert(PrettyPrintRecordKey::from(key).into_owned(), failed_peers);
            }
        }

        if !failed.is_empty() {
            error!("Verification failed for {:?} entries", failed.len());
            println!("Verification failed for {:?} entries", failed.len());

            failed.iter().for_each(|(key, failed_peers)| {
                failed_peers
                    .iter()
                    .for_each(|peer| println!("Record {key:?} is not stored inside {peer:?}",));
            });
            println!("State of each node:");
            record_holders.iter().for_each(|(key, node_index)| {
                println!("Record {key:?} is currently held by node indexes {node_index:?}");
            });
            println!("Node index map:");
            all_peers
                .iter()
                .enumerate()
                .for_each(|(idx, peer)| println!("{idx} : {peer:?}"));
            verification_attempts += 1;
            println!("Sleeping before retrying verification");
            tokio::time::sleep(REVERIFICATION_DELAY).await;
        } else {
            // if successful, break out of the loop
            break;
        }
    }

    if !failed.is_empty() {
        println!("Verification failed after {VERIFICATION_ATTEMPTS} times");
        Err(eyre!("Verification failed for: {failed:?}"))
    } else {
        println!("All the Records have been verified!");
        Ok(())
    }
}

// Generate random Chunks and store them to the Network
async fn store_chunks(client: Client, chunk_count: usize, wallet_dir: PathBuf) -> Result<()> {
    let start = Instant::now();
    let mut rng = OsRng;
    let files_api = FilesApi::new(client, wallet_dir);

    let mut uploaded_chunks_count = 0;
    loop {
        if uploaded_chunks_count >= chunk_count {
            break;
        }

        let chunks_dir = TempDir::new()?;

        let random_bytes: Vec<u8> = ::std::iter::repeat(())
            .map(|()| rng.gen::<u8>())
            .take(CHUNK_SIZE)
            .collect();

        let file_path = chunks_dir.join("random_content");
        let mut output_file = File::create(file_path.clone())?;
        output_file.write_all(&random_bytes)?;

        let (head_chunk_addr, _data_map, _file_size, chunks) =
            FilesApi::chunk_file(&file_path, chunks_dir.path(), true)?;

        println!(
            "Paying storage for ({}) new Chunk/s of file ({} bytes) at {head_chunk_addr:?}",
            chunks.len(),
            random_bytes.len()
        );

        let key =
            PrettyPrintRecordKey::from(&RecordKey::new(&head_chunk_addr.xorname())).into_owned();
        let mut file_upload = FilesUpload::new(files_api.clone())
            .set_show_holders(true)
            .set_verify_store(false);
        file_upload.upload_chunks(chunks).await?;
        uploaded_chunks_count += 1;

        println!("Stored Chunk with {head_chunk_addr:?} / {key:?}");
    }

    println!(
        "{chunk_count:?} Chunks were stored in {:?}",
        start.elapsed()
    );

    // to make sure the last chunk was stored
    tokio::time::sleep(Duration::from_secs(10)).await;

    Ok(())
}
