use borsh_ext::BorshSerializeExt;
use namada_core::proto::Tx;
use namada_core::types::address::Address;
use namada_core::types::hash::Hash;
use namada_core::types::key::common;
use namada_core::types::token::DenominatedAmount;

use super::GlobalArgs;
use crate::transaction;

const TX_TRANSFER_WASM: &str = "tx_transfer.wasm";

pub struct Transfer(Tx);

impl Transfer {
    /// Build a raw Transfer transaction from the given parameters
    pub fn new(
        source: Address,
        target: Address,
        token: Address,
        amount: DenominatedAmount,
        key: Option<String>,
        // FIXME: handle masp here
        shielded: Option<Hash>,
        args: GlobalArgs,
    ) -> Self {
        let init_proposal = namada_core::types::token::Transfer {
            source,
            target,
            token,
            amount,
            key,
            shielded,
        };

        Self(transaction::build_tx(
            args,
            init_proposal.serialize_to_vec(),
            TX_TRANSFER_WASM.to_string(),
        ))
    }

    /// Get the bytes to sign for the given transaction
    pub fn get_sign_bytes(&self) -> Vec<Hash> {
        transaction::get_sign_bytes(&self.0)
    }

    /// Attach the provided signatures to the tx
    pub fn attach_signatures(
        self,
        signer: common::PublicKey,
        signature: common::Signature,
    ) -> Self {
        Self(transaction::attach_raw_signatures(
            self.0, signer, signature,
        ))
    }
}
