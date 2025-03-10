// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod spend_dag;

use super::{
    error::{Error, Result},
    Client,
};

use futures::future::join_all;
use sn_transfers::{
    CashNoteRedemption, SignedSpend, SpendAddress, Transfer, WalletError, WalletResult,
    NETWORK_ROYALTIES_PK,
};
use std::{collections::BTreeSet, iter::Iterator, path::Path};

impl Client {
    /// Verify that a spend is valid on the network.
    /// Optionally verify its ancestors as well, all the way to genesis (might take a LONG time)
    ///
    /// Prints progress on stdout.
    ///
    /// When verifying all the way back to genesis, it only verifies Spends that are ancestors of the given Spend,
    /// ignoring all other branches.
    ///
    /// This is how the DAG it follows could look like:
    /// ```text
    ///              ... --
    ///                     \
    ///              ... ----                  ... --    
    ///                       \                       \
    /// Spend0 -> Spend1 -----> Spend2 ---> Spend5 ---> Spend2 ---> Genesis
    ///                   \                           /
    ///                    ---> Spend3 ---> Spend6 ->
    ///                     \            /
    ///                      -> Spend4 ->
    ///                                /
    ///                            ...
    ///
    /// depth0    depth1        depth2      depth3      depth4      depth5
    /// ```
    ///
    /// This function will return an error if any spend in the way is invalid.
    pub async fn verify_spend(&self, addr: SpendAddress, to_genesis: bool) -> WalletResult<()> {
        let first_spend = self
            .get_spend_from_network(addr)
            .await
            .map_err(|err| WalletError::CouldNotVerifyTransfer(err.to_string()))?;

        if !to_genesis {
            return Ok(());
        }

        // use iteration instead of recursion to avoid stack overflow
        let mut txs_to_verify = BTreeSet::from_iter([first_spend.spend.parent_tx]);
        let mut depth = 0;
        let mut verified_tx = BTreeSet::new();
        let start = std::time::Instant::now();

        while !txs_to_verify.is_empty() {
            let mut next_gen_tx = BTreeSet::new();

            for parent_tx in txs_to_verify {
                let parent_tx_hash = parent_tx.hash();
                let parent_keys = parent_tx.inputs.iter().map(|input| input.unique_pubkey);
                let addrs_to_verify = parent_keys.map(|k| SpendAddress::from_unique_pubkey(&k));
                debug!("Depth {depth} - Verifying parent Tx : {parent_tx_hash:?}");

                // get all parent spends in parallel
                let tasks: Vec<_> = addrs_to_verify
                    .into_iter()
                    .map(|a| self.get_spend_from_network(a))
                    .collect();
                let spends = join_all(tasks).await
                    .into_iter()
                    .collect::<Result<BTreeSet<_>>>()
                    .map_err(|err| WalletError::CouldNotVerifyTransfer(format!("at depth {depth} - Failed to get spends from network for parent Tx {parent_tx_hash:?}: {err}")))?;
                debug!(
                    "Depth {depth} - Got {:?} spends for parent Tx: {parent_tx_hash:?}",
                    spends.len()
                );
                trace!("Spends for {parent_tx_hash:?} - {spends:?}");

                // check if we reached the genesis Tx
                if parent_tx == sn_transfers::GENESIS_CASHNOTE.src_tx
                    && spends
                        .iter()
                        .all(|s| s.spend.unique_pubkey == sn_transfers::GENESIS_CASHNOTE.id)
                    && spends.len() == 1
                {
                    debug!("Depth {depth} - Reached genesis Tx on one branch: {parent_tx_hash:?}");
                    verified_tx.insert(parent_tx_hash);
                    continue;
                }

                // verify tx with those spends
                parent_tx
                    .verify_against_inputs_spent(&spends)
                    .map_err(|err| WalletError::CouldNotVerifyTransfer(format!("at depth {depth} - Failed to verify parent Tx {parent_tx_hash:?}: {err}")))?;
                verified_tx.insert(parent_tx_hash);
                debug!("Depth {depth} - Verified parent Tx: {parent_tx_hash:?}");

                // add new parent spends to next gen
                next_gen_tx.extend(spends.into_iter().map(|s| s.spend.parent_tx));
            }

            // only verify parents we haven't already verified
            txs_to_verify = next_gen_tx
                .into_iter()
                .filter(|tx| !verified_tx.contains(&tx.hash()))
                .collect();

            depth += 1;
            let elapsed = start.elapsed();
            let n = verified_tx.len();
            println!("Now at depth {depth} - Verified {n} transactions in {elapsed:?}");
        }

        let elapsed = start.elapsed();
        let n = verified_tx.len();
        println!("Verified all the way to genesis! Through {depth} generations, verifying {n} transactions in {elapsed:?}");
        Ok(())
    }

    /// This function does the opposite of verify_spend.
    /// It recursively follows the descendants of a Spend, all the way to unspent Transaction Outputs (UTXOs).
    ///
    /// Prints progress on stdout
    ///
    /// Starting from Genesis, this amounts to Auditing the entire currency.
    /// This is how the DAG it follows could look like:
    ///
    /// ```text
    ///                                   -> Spend7 ---> UTXO_11
    ///                                 /
    /// Genesis -> Spend1 -----> Spend2 ---> Spend5 ---> UTXO_10
    ///                   \
    ///                     ---> Spend3 ---> Spend6 ---> UTXO_9
    ///                     \
    ///                       -> Spend4 ---> UTXO_8
    ///
    /// gen0       gen1          gen2        gen3
    ///
    /// ```
    ///
    /// This function will return the UTXOs (Spend addresses not spent yet)
    /// Future calls to this function could start from those UTXOs to avoid
    /// re-checking all previously checked branches.
    pub async fn follow_spend(
        &self,
        spend_addr: SpendAddress,
        find_royalties: bool,
        root_dir: &Path,
    ) -> WalletResult<BTreeSet<SpendAddress>> {
        let first_spend = self
            .get_spend_from_network(spend_addr)
            .await
            .map_err(|err| WalletError::CouldNotVerifyTransfer(err.to_string()))?;
        println!("Generation 0 - Found first spend: {spend_addr:#?}");

        // use iteration instead of recursion to avoid stack overflow
        let mut txs_to_follow = BTreeSet::from_iter([first_spend.spend.spent_tx]);
        let mut all_utxos = BTreeSet::new();
        let mut verified_tx = BTreeSet::new();
        let mut gen = 0;
        let start = std::time::Instant::now();

        while !txs_to_follow.is_empty() {
            let mut next_gen_tx = BTreeSet::new();
            let mut next_gen_spends = BTreeSet::new();
            let mut next_gen_utxos = BTreeSet::new();

            for descendant_tx in txs_to_follow.iter() {
                let descendant_tx_hash = descendant_tx.hash();
                let descendant_keys = descendant_tx
                    .outputs
                    .iter()
                    .map(|output| output.unique_pubkey);
                let addrs_to_follow = descendant_keys.map(|k| SpendAddress::from_unique_pubkey(&k));
                debug!("Gen {gen} - Following descendant Tx : {descendant_tx_hash:?}");

                // get all descendant spends in parallel
                let tasks: Vec<_> = addrs_to_follow
                    .into_iter()
                    .map(|a| self.get_spend_from_network(a))
                    .collect();
                let spends_res = join_all(tasks).await.into_iter().collect::<Vec<_>>();

                // split spends into utxos and spends
                let (utxos, spends) = split_utxos_and_spends(spends_res)
                    .map_err(|err| WalletError::CouldNotVerifyTransfer(format!("at gen {gen} - Failed to get spends from network for descendant Tx {descendant_tx_hash:?}: {err}")))?;
                debug!("Gen {gen} - Got {:?} spends and {:?} utxos for descendant Tx: {descendant_tx_hash:?}", spends.len(), utxos.len());
                trace!("Spends for {descendant_tx_hash:?} - {spends:?}");
                next_gen_utxos.extend(utxos);
                next_gen_spends.extend(
                    spends
                        .iter()
                        .map(|s| SpendAddress::from_unique_pubkey(&s.spend.unique_pubkey)),
                );

                // look for royalties
                self.redeem_royalties(find_royalties, &spends, root_dir)
                    .await;

                // add new descendant spends to next gen
                next_gen_tx.extend(spends.into_iter().map(|s| s.spend.spent_tx));
            }

            // print stats
            gen += 1;
            let elapsed = start.elapsed();
            let u = next_gen_utxos.len();
            let s = next_gen_spends.len();
            println!("Generation {gen} - Found {u} UTXOs and {s} Spends in {elapsed:?}");
            debug!("Generation {gen} - UTXOs: {:#?}", next_gen_utxos);
            debug!("Generation {gen} - Spends: {:#?}", next_gen_spends);
            all_utxos.extend(next_gen_utxos);

            // only verify tx we haven't already verified
            verified_tx.extend(txs_to_follow.iter().map(|tx| tx.hash()));
            txs_to_follow = next_gen_tx
                .into_iter()
                .filter(|tx| !verified_tx.contains(&tx.hash()))
                .collect();
        }

        let elapsed = start.elapsed();
        let n = all_utxos.len();
        let tx = verified_tx.len();
        println!("Finished auditing! Through {gen} generations, found {n} UTXOs and verified {tx} Transactions in {elapsed:?}");
        Ok(all_utxos)
    }

    /// This function serves as a proof of concept of royalties collection
    async fn redeem_royalties(
        &self,
        find_royalties: bool,
        spends: &Vec<SignedSpend>,
        root_dir: &Path,
    ) {
        if !find_royalties {
            return;
        }

        // Turn those royalties into a Transfer and redeems them
        // This involves encrypting/decrypting the Transfer, which is a waste
        // This involves re-verifying, which we don't need as we're already auditing
        // This prints out a Transfer for each royalty, which is not ideal but keeps the transfers reasonnably small
        // This might print out duplicates as it doens't keep track of what's coming, but that's ok as the cli will know what to do with them
        // It is sub-optimial, but it's a working proof of concept that will need to be refined.
        // If we decide to adopt this, we will need to turn this indentation space ship into a proper piece of optimized code.
        let mut count = 0;
        let royalties_key = *NETWORK_ROYALTIES_PK;
        let mut wallet =
            sn_transfers::LocalWallet::load_from(root_dir).expect("Failed to load wallet");
        for spend in spends {
            for derivation_idx in spend.spend.network_royalties.iter() {
                count += 1;
                let spend_addr = SpendAddress::from_unique_pubkey(&spend.spend.unique_pubkey);
                let royalties = vec![CashNoteRedemption::new(*derivation_idx, spend_addr)];
                match Transfer::create(royalties, royalties_key) {
                    Ok(transfer) => {
                        let unique_key = royalties_key.new_unique_pubkey(derivation_idx);
                        println!("Identified royalties token: {unique_key:?}");
                        match self.receive(&transfer, &wallet).await {
                            Ok(cn) => {
                                println!(
                                    "Successfully received royalties CashNotes, depositing..."
                                );
                                let old_balance = wallet.balance();
                                if let Err(e) = wallet.deposit_and_store_to_disk(&cn) {
                                    println!("Failed to store redeemed royalties CashNotes: {e}");
                                } else {
                                    let new_balance = wallet.balance();
                                    println!("Successfully deposited royalties CashNotes, new balance: {new_balance} (was {old_balance})");
                                }
                            }
                            Err(e) => {
                                println!("Failed to redeem royalties CashNotes: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        println!("Error creating royalties transfer: {e}");
                    }
                }
            }
        }

        println!("Found {count:?} royalties");
    }
}

fn split_utxos_and_spends(
    spends_res: Vec<Result<SignedSpend>>,
) -> Result<(Vec<SpendAddress>, Vec<SignedSpend>)> {
    let mut utxos = Vec::new();
    let mut spends = Vec::new();

    for res in spends_res {
        match res {
            Ok(spend) => {
                spends.push(spend);
            }
            Err(Error::MissingSpendRecord(addr)) => {
                utxos.push(addr);
            }
            Err(err) => {
                warn!("Error while following spends: {err}");
                return Err(err);
            }
        }
    }

    Ok((utxos, spends))
}
