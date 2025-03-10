// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{claim_genesis, send_tokens};
use color_eyre::eyre::{eyre, Result};
use sn_client::Client;
use sn_transfers::{LocalWallet, NanoTokens};
use std::path::{self, Path, PathBuf};
use tiny_http::{Response, Server};
use tracing::{debug, error, trace};

/// Run the faucet server.
///
/// This will listen on port 8000 and send a transfer of tokens as response to any GET request.
///
/// # Example
///
/// ```bash
/// # run faucet server
/// cargo run  --features="local-discovery" --bin faucet --release -- server
///
/// # query faucet server for money for our address `get local wallet address`
/// curl "localhost:8000/`cargo run  --features="local-discovery"  --bin safe --release  wallet address | tail -n 1`" > transfer_hex
///
/// # receive transfer with our wallet
/// cargo run  --features="local-discovery" --bin safe --release  wallet receive --file transfer_hex
///
/// # balance should be updated
/// ```
pub async fn run_faucet_server(client: &Client) -> Result<()> {
    claim_genesis(client).await.map_err(|err| {
        println!("Faucet Server couldn't start as we failed to claim Genesis");
        eprintln!("Faucet Server couldn't start as we failed to claim Genesis");
        error!("Faucet Server couldn't start as we failed to claim Genesis");
        err
    })?;
    startup_server(client).await
}

pub async fn restart_faucet_server(client: &Client) -> Result<()> {
    let root_dir = get_test_faucet_data_dir_path()?;
    println!("Loading the previous wallet at {root_dir:?}");
    debug!("Loading the previous wallet at {root_dir:?}");

    deposit(&root_dir)?;

    println!("Previous wallet loaded");
    debug!("Previous wallet loaded");

    startup_server(client).await
}

async fn startup_server(client: &Client) -> Result<()> {
    let server =
        Server::http("0.0.0.0:8000").map_err(|err| eyre!("Failed to start server: {err}"))?;

    // This println is used in sn_testnet to wait for the faucet to start.
    println!("Starting http server listening on port 8000...");
    debug!("Starting http server listening on port 8000...");
    for request in server.incoming_requests() {
        println!(
            "received request! method: {:?}, url: {:?}, headers: {:?}",
            request.method(),
            request.url(),
            request.headers()
        );
        trace!(
            "received request! method: {:?}, url: {:?}, headers: {:?}",
            request.method(),
            request.url(),
            request.headers()
        );
        let key = request.url().trim_matches(path::is_separator);

        match send_tokens(client, "100", key).await {
            Ok(transfer) => {
                println!("Sent tokens to {key}");
                debug!("Sent tokens to {key}");
                let response = Response::from_string(transfer);
                let _ = request.respond(response).map_err(|err| {
                    eprintln!("Failed to send response: {err}");
                    error!("Failed to send response: {err}");
                });
            }
            Err(err) => {
                eprintln!("Failed to send tokens to {key}: {err}");
                error!("Failed to send tokens to {key}: {err}");
                let response = Response::from_string(format!("Failed to send tokens: {err}"));
                let _ = request
                    .respond(response.with_status_code(500))
                    .map_err(|err| eprintln!("Failed to send response: {err}"));
            }
        }
    }
    Ok(())
}

fn get_test_faucet_data_dir_path() -> Result<PathBuf> {
    let home_dirs = Path::new("/home/safe/.local/share/safe/test_faucet");
    std::fs::create_dir_all(home_dirs)?;
    Ok(home_dirs.to_path_buf())
}

fn deposit(root_dir: &Path) -> Result<()> {
    let mut wallet = LocalWallet::load_from(root_dir)?;

    let previous_balance = wallet.balance();

    wallet.try_load_cash_notes()?;

    let deposited = NanoTokens::from(wallet.balance().as_nano() - previous_balance.as_nano());
    if deposited.is_zero() {
        println!("Nothing deposited.");
    } else if let Err(err) = wallet.deposit_and_store_to_disk(&vec![]) {
        println!("Failed to store deposited ({deposited}) amount: {err:?}");
    } else {
        println!("Deposited {deposited}.");
    }

    Ok(())
}
