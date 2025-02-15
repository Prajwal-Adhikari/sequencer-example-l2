// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

use commit::{Commitment, Committable};
use ethers::abi::Address;
use jf_primitives::merkle_tree::namespaced_merkle_tree::NamespaceProof;
use sequencer::{NMTRoot, NamespaceProofType, Vm};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::RollupError;
use crate::prover::Proof;
use crate::transaction::SignedTransaction;
use crate::RollupVM;

pub type Amount = u64;
pub type Nonce = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Account {
    balance: Amount,
    nonce: Nonce,
}

#[derive(Debug, Clone)]
pub struct State {
    // Account state, represented as a BTreeMap so that we can obtain a canonical serialization of the data structure for the state commitment
    // A live rollup would likely represent accounts as a Sparse Merkle Tree instead of a BTreeMap.
    // Rollup clients would then be able to use merkle proofs to authenticate a subset of user balances
    // without knowledge of the entire account state. Such "light clients" are less constrained by bandwidth
    // because they do not need to constantly sync up with a full node.
    accounts: BTreeMap<Address, Account>,
    nmt_comm: Option<Commitment<NMTRoot>>, // Commitment to the most recent transaction NMT
    prev_state_commitment: Option<Commitment<State>>, // Previous state commitment, used to create a chain linking state committments
    pub(crate) vm: RollupVM,
}

impl Committable for State {
    /// Commits the current state by creating a serialized commitment object.
    ///
    /// # Returns
    /// A `Commitment<State>` representing the current state, which includes:
    /// - Serialized account data
    /// - Block hash of the current state
    /// - Previous state commitments
    /// - The VM ID used in the state.
    fn commit(&self) -> Commitment<State> {
        let serialized_accounts =
            serde_json::to_string(&self.accounts).expect("Serialization should not fail");

        commit::RawCommitmentBuilder::new("State Commitment")
            .array_field(
                "block_hash",
                &self
                    .nmt_comm
                    .iter()
                    .cloned()
                    .map(Commitment::<NMTRoot>::from)
                    .collect::<Vec<_>>(),
            )
            .array_field(
                "prev_state_commitment",
                &self
                    .prev_state_commitment
                    .iter()
                    .cloned()
                    .map(Commitment::<State>::from)
                    .collect::<Vec<_>>(),
            )
            .var_size_field("accounts", serialized_accounts.as_bytes())
            .u64_field("VM ID", self.vm.id().into())
            .finalize()
    }
}

impl State {
    /// Create new VM state seeded with some initial balances.
    ///
    /// # Parameters
    /// - `initial_balances`: An iterator yielding tuples of addresses and their corresponding initial amounts.
    /// - `vm`: A RollupVM instance representing the virtual machine associated with this state.
    ///
    /// # Returns
    /// A new instance of `State` containing the initialized accounts.
    pub fn from_initial_balances(
        initial_balances: impl IntoIterator<Item = (Address, Amount)>,
        vm: RollupVM,
    ) -> Self {
        let mut accounts = BTreeMap::new();
        for (addr, amount) in initial_balances.into_iter() {
            accounts.insert(
                addr,
                Account {
                    balance: amount,
                    nonce: 0,
                },
            );
        }
        State {
            accounts,
            nmt_comm: None,
            prev_state_commitment: None,
            vm,
        }
    }

    /// If the transaction is valid, transition the state and return the new state with updated balances.
    ///
    /// A transaction is valid iff
    /// 1) The signature on the transaction
    /// 2) The nonce of the transaction is greater than the sender nonce (this prevent replay attacks)
    /// 3) The sender has a high enough balance to cover the transfer amount
    pub fn apply_transaction(
        &mut self,
        transaction: &SignedTransaction,
    ) -> Result<(), RollupError> {
        // 1)
        let sender = transaction.recover()?;
        let destination = transaction.transaction.destination;
        let next_nonce = transaction.transaction.nonce;
        let transfer_amount = transaction.transaction.amount;
        // Fetch the sender's account and check if it exists
        let Account {
            nonce: prev_nonce,
            balance: sender_balance,
        } = self
            .accounts
            .get_mut(&sender)
            .ok_or(RollupError::InsufficientBalance { address: sender })?;

        // Validate nonce
        if next_nonce != *prev_nonce + 1 {
            return Err(RollupError::InvalidNonce {
                address: sender,
                expected: *prev_nonce + 1,
                actual: next_nonce,
            });
        }

        // Validate balance
        if transfer_amount > *sender_balance {
            return Err(RollupError::InsufficientBalance { address: sender });
        }

        // Transaction is valid, return the updated state
        *sender_balance -= transfer_amount;
        *prev_nonce = next_nonce;
        let Account {
            balance: destination_balance,
            ..
        } = self.accounts.entry(destination).or_default();
        *destination_balance += transfer_amount;

        tracing::info!("Applied transaction {next_nonce} for {sender}");
        Ok(())
    }

    /// Fetch the balance of an address
    pub fn get_balance(&self, address: &Address) -> Amount {
        self.accounts
            .get(address)
            .map(|account| account.balance)
            .unwrap_or(0)
    }

    /// Fetch the nonce of an address
    pub fn get_nonce(&self, address: &Address) -> Nonce {
        self.accounts
            .get(address)
            .map(|account| account.nonce)
            .unwrap_or(0)
    }

    /// Execute a block of transactions, updating the state and generating a proof.
    ///
    /// # Parameters
    /// - `nmt_root`: The root of the NMT for this block.
    /// - `namespace_proof`: Proofs related to the namespace.
    ///
    /// # Returns
    /// A `Proof` object representing the state after executing the block.
    pub(crate) async fn execute_block(
        &mut self,
        nmt_root: NMTRoot,
        namespace_proof: NamespaceProofType,
    ) -> Proof {
        let state_commitment = self.commit();
        let transactions = namespace_proof.get_namespace_leaves();
        for txn in transactions {
            if let Some(rollup_txn) = txn.as_vm(&self.vm) {
                let res = self.apply_transaction(&rollup_txn);
                if let Err(err) = res {
                    tracing::error!("Transaction invalid: {}", err)
                }
            } else {
                tracing::error!("NMT transaction is malformed")
            }
        }
        self.nmt_comm = Some(nmt_root.commit());
        self.prev_state_commitment = Some(state_commitment);

        Proof::generate(
            nmt_root,
            self.commit(),
            self.prev_state_commitment.unwrap(),
            namespace_proof,
            &self.vm,
        )
    }
}
#[cfg(test)]
mod tests {
    use crate::transaction::Transaction;

    use ethers::signers::{LocalWallet, Signer};

    use super::*;
    #[async_std::test]
    async fn smoke_test() {
        let mut rng = rand::thread_rng();
        let vm = RollupVM::new(1.into());
        let alice = LocalWallet::new(&mut rng);
        let bob = LocalWallet::new(&mut rng);
        let seed_data = [(alice.address(), 100), (bob.address(), 100)];
        let mut state = State::from_initial_balances(seed_data, vm);
        let mut transaction = Transaction {
            amount: 110,
            destination: bob.address(),
            nonce: 1,
        };

        // Try to overspend
        let mut signed_transaction = SignedTransaction::new(transaction.clone(), &alice).await;
        let err = state
            .clone()
            .apply_transaction(&signed_transaction)
            .expect_err("Invalid transaction should throw error.");
        assert_eq!(
            err,
            RollupError::InsufficientBalance {
                address: alice.address()
            }
        );

        // Now spend an valid amount
        transaction.amount = 50;
        signed_transaction = SignedTransaction::new(transaction, &alice).await;
        state
            .apply_transaction(&signed_transaction)
            .expect("Valid transaction should transition state");
        let bob_balance = state.get_balance(&bob.address());
        assert_eq!(bob_balance, 150);

        // Now try to replay the transaction
        let err = state
            .apply_transaction(&signed_transaction)
            .expect_err("Invalid transaction should throw error.");
        assert_eq!(
            err,
            RollupError::InvalidNonce {
                address: alice.address(),
                expected: 2,
                actual: 1,
            }
        );
    }
}
