// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
#![allow(clippy::mutable_key_type)] // for Bytes in NetworkAddress

use crate::{storage::RecordType, NetworkAddress};
use serde::{Deserialize, Serialize};
// TODO: remove this dependency and define these types herein.
pub use sn_transfers::Hash;

/// Data and CashNote cmds - recording spends or creating, updating, and removing data.
///
/// See the [`protocol`] module documentation for more details of the types supported by the Safe
/// Network, and their semantics.
///
/// [`protocol`]: crate
#[allow(clippy::large_enum_variant)]
#[derive(Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum Cmd {
    /// Write operation to notify peer fetch a list of [`NetworkAddress`] from the holder.
    ///
    /// [`NetworkAddress`]: crate::NetworkAddress
    Replicate {
        /// Holder of the replication keys.
        holder: NetworkAddress,
        /// Keys of copy that shall be replicated.
        keys: Vec<(NetworkAddress, RecordType)>,
    },
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Replicate { holder, keys } => {
                let first_ten_keys: Vec<_> = keys.iter().take(10).collect();
                f.debug_struct("Cmd::Replicate")
                    .field("holder", holder)
                    .field("keys_len", &keys.len())
                    .field("first_ten_keys", &first_ten_keys)
                    .finish()
            }
        }
    }
}

impl Cmd {
    /// Used to send a cmd to the close group of the address.
    pub fn dst(&self) -> NetworkAddress {
        match self {
            Cmd::Replicate { holder, .. } => holder.clone(),
        }
    }
}

impl std::fmt::Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Replicate { holder, keys } => {
                write!(
                    f,
                    "Cmd::Replicate({:?} has {} keys)",
                    holder.as_peer_id(),
                    keys.len()
                )
            }
        }
    }
}
