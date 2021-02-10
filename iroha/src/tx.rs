//! This module contains Transaction related functionality of the Iroha.
//!
//! `Transaction` is the start of the Transaction lifecycle.

use crate::{expression::Evaluate, isi::Execute, permissions::PermissionsValidatorBox, prelude::*};
use iroha_data_model::prelude::*;
use iroha_derive::Io;
use parity_scale_codec::{Decode, Encode};
use std::{
    cmp::min,
    time::{Duration, SystemTime},
};

/// `AcceptedTransaction` represents a transaction accepted by iroha peer.
#[derive(Clone, Debug, Io, Encode, Decode)]
pub struct AcceptedTransaction {
    /// Payload of this transaction.
    pub payload: Payload,
    /// Signatures for this transaction.
    pub signatures: Vec<Signature>,
}

impl AcceptedTransaction {
    /// Accepts transaction
    pub fn from_transaction(
        transaction: Transaction,
        max_instruction_number: usize,
    ) -> Result<AcceptedTransaction, String> {
        transaction
            .check_instruction_len(max_instruction_number)
            .map_err(|e| format!("Failed to accept transaction: {}", e))?;

        for signature in &transaction.signatures {
            if let Err(e) = signature.verify(transaction.hash().as_ref()) {
                return Err(format!("Failed to verify signatures: {}", e));
            }
        }

        Ok(Self {
            payload: transaction.payload,
            signatures: transaction.signatures,
        })
    }

    /// Calculate transaction `Hash`.
    pub fn hash(&self) -> Hash {
        let bytes: Vec<u8> = self.payload.clone().into();
        Hash::new(&bytes)
    }

    /// Checks if this transaction is waiting longer than specified in `transaction_time_to_live` from `QueueConfiguration` or `time_to_live_ms` of this transaction.
    /// Meaning that the transaction will be expired as soon as the lesser of the specified TTLs was reached.
    pub fn is_expired(&self, transaction_time_to_live: Duration) -> bool {
        let current_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to get System Time.");
        (current_time - Duration::from_millis(self.payload.creation_time))
            > min(
                Duration::from_millis(self.payload.time_to_live_ms),
                transaction_time_to_live,
            )
    }

    /// Move transaction lifecycle forward by checking an ability to apply instructions to the
    /// `WorldStateView`.
    ///
    /// Returns `Ok(ValidTransaction)` if succeeded and `Err(String)` if failed.
    pub fn validate(
        self,
        world_state_view: &WorldStateView,
        permissions_validator: &PermissionsValidatorBox,
        is_genesis: bool,
    ) -> Result<ValidTransaction, RejectedTransaction> {
        let mut world_state_view_temp = world_state_view.clone();
        let account_id = self.payload.account_id.clone();
        if !is_genesis && account_id == <Account as Identifiable>::Id::genesis_account() {
            return Err(
                self.reject("Genesis account can sign only transactions in the genesis block.")
            );
        }
        for signature in &self.signatures {
            if let Err(e) = signature.verify(self.hash().as_ref()) {
                return Err(self
                    .clone()
                    .reject(&format!("Failed to verify signatures: {}", e)));
            }
        }
        if !self
            .check_signature_condition(world_state_view)
            .map_err(|error| self.clone().reject(&error))?
        {
            return Err(self.reject("Signature condition not satisfied."));
        }
        for instruction in &self.payload.instructions {
            let account_id = self.payload.account_id.clone();
            world_state_view_temp = instruction
                .clone()
                .execute(account_id.clone(), &world_state_view_temp)
                .map_err(|error| self.clone().reject(&error))?;
            if !is_genesis {
                permissions_validator
                    .check_instruction(account_id.clone(), instruction.clone(), &world_state_view)
                    .map_err(|error| self.clone().reject(&error))?;
            }
        }
        Ok(ValidTransaction {
            payload: self.payload,
            signatures: self.signatures,
        })
    }

    /// Checks that the signatures of this transaction satisfy the signature condition specified in the account.
    pub fn check_signature_condition(
        &self,
        world_state_view: &WorldStateView,
    ) -> Result<bool, String> {
        let account_id = self.payload.account_id.clone();
        world_state_view
            .read_account(&account_id)
            .ok_or_else(|| format!("Account with id {} not found", account_id))?
            .check_signature_condition(&self.signatures)
            .evaluate(world_state_view, &Context::new())
    }

    /// Rejects transaction with the `rejection_reason`.
    pub fn reject(self, rejection_reason: &str) -> RejectedTransaction {
        RejectedTransaction {
            payload: self.payload,
            signatures: self.signatures,
            rejection_reason: rejection_reason.to_string(),
        }
    }
}

impl From<AcceptedTransaction> for Transaction {
    fn from(transaction: AcceptedTransaction) -> Self {
        Transaction {
            payload: transaction.payload,
            signatures: transaction.signatures,
        }
    }
}

/// `ValidTransaction` represents trustfull Transaction state.
#[derive(Clone, Debug, Io, Encode, Decode)]
pub struct ValidTransaction {
    payload: Payload,
    signatures: Vec<Signature>,
}

impl ValidTransaction {
    /// Apply instructions to the `WorldStateView`.
    pub fn proceed(&self, world_state_view: &mut WorldStateView) -> Result<(), String> {
        let mut world_state_view_temp = world_state_view.clone();
        for instruction in &self.payload.instructions {
            world_state_view_temp = instruction
                .clone()
                .execute(self.payload.account_id.clone(), &world_state_view_temp)?;
        }
        *world_state_view = world_state_view_temp;
        Ok(())
    }

    /// Calculate transaction `Hash`.
    pub fn hash(&self) -> Hash {
        let bytes: Vec<u8> = self.payload.clone().into();
        Hash::new(&bytes)
    }
}

impl From<ValidTransaction> for AcceptedTransaction {
    fn from(transaction: ValidTransaction) -> Self {
        AcceptedTransaction {
            payload: transaction.payload,
            signatures: transaction.signatures,
        }
    }
}

/// `RejectedTransaction` represents transaction rejected by some validator at some stage of the pipeline.
#[derive(Clone, Debug, Io, Encode, Decode)]
pub struct RejectedTransaction {
    payload: Payload,
    signatures: Vec<Signature>,
    // TODO: Convert errors in the tx pipeline to enums to simplify translation to other languages.
    /// The reason for rejecting this tranaction during the validation pipeline.
    pub rejection_reason: String,
}

impl RejectedTransaction {
    /// Calculate transaction `Hash`.
    pub fn hash(&self) -> Hash {
        let bytes: Vec<u8> = self.payload.clone().into();
        Hash::new(&bytes)
    }
}

impl From<RejectedTransaction> for AcceptedTransaction {
    fn from(transaction: RejectedTransaction) -> Self {
        AcceptedTransaction {
            payload: transaction.payload,
            signatures: transaction.signatures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Configuration, init, permissions::AllowAll};
    use iroha_data_model::transaction::MAX_INSTRUCTION_NUMBER;
    use std::collections::BTreeSet;

    const CONFIGURATION_PATH: &str = "tests/test_config.json";

    #[test]
    fn hash_should_be_the_same() {
        let mut config =
            Configuration::from_path(CONFIGURATION_PATH).expect("Failed to load configuration.");
        let tx = Transaction::new(vec![], AccountId::new("root", "global"), 1000);
        let tx_hash = tx.hash();
        let root_key_pair = &KeyPair::generate().expect("Failed to generate key pair.");
        config.init_configuration.root_public_key = root_key_pair.public_key.clone();
        let signed_tx = tx.sign(&root_key_pair).expect("Failed to sign.");
        let signed_tx_hash = signed_tx.hash();
        let accepted_tx =
            AcceptedTransaction::from_transaction(signed_tx, 4096).expect("Failed to accept.");
        let accepted_tx_hash = accepted_tx.hash();
        let valid_tx_hash = accepted_tx
            .validate(
                &WorldStateView::new(World::with(init::domains(&config), BTreeSet::new())),
                &AllowAll.into(),
                false,
            )
            .expect("Failed to validate.")
            .hash();
        assert_eq!(tx_hash, signed_tx_hash);
        assert_eq!(tx_hash, accepted_tx_hash);
        assert_eq!(tx_hash, valid_tx_hash);
    }

    #[test]
    fn transaction_not_accepted() {
        let inst = FailBox {
            message: "Will fail".to_owned(),
        };
        let tx = Transaction::new(
            vec![inst.into(); MAX_INSTRUCTION_NUMBER + 1],
            AccountId::new("root", "global"),
            1000,
        );
        let result: Result<AcceptedTransaction, _> =
            AcceptedTransaction::from_transaction(tx, 4096);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Failed to accept transaction: Too many instructions in payload".to_owned()
        );
    }
}
