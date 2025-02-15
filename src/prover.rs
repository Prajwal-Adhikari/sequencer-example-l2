// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

extern crate derive_more;
use ark_serialize::SerializationError;
use commit::{Commitment, Committable};
use contract_bindings::example_rollup as bindings;
use derive_more::Into;
use jf_primitives::merkle_tree::namespaced_merkle_tree::NamespaceProof;
use sequencer::{NMTRoot, NamespaceProofType, Vm};
use sequencer_utils::{commitment_to_u256, u256_to_commitment};
use snafu::Snafu;

use crate::{state::State, RollupVM};

/// An error that occurs while generating proofs.
#[derive(Clone, Debug, Snafu)]
pub enum ProofError {
    // Raised when proofs are not provided in the correct order (i.e. when consecutive proofs do not align).
    #[snafu(display("Proofs out of order at position {position} in batch proof. Previous proof ends in {new_state} but next proof starts in {old_state}."))]
    OutOfOrder {
        position: usize,
        new_state: Commitment<State>,
        old_state: Commitment<State>,
    },
}

/// A mock proof that state_commitment represents a valid state transition from
/// previous_state_commitment when the transactions in a given block are applied.
#[derive(Debug, Clone)]
pub(crate) struct Proof {
    block: Commitment<NMTRoot>,
    old_state: Commitment<State>,
    new_state: Commitment<State>,
}

impl Proof {
    /// The namespace proof is a private input to the mock proof, showing that
    /// the proof of the state transition accounts for every transaction in the rollup's namespace
    ///
    /// Transaction data comes from the 'get_namespaced_leaves' method of the NamespaceProof interface.
    /// A real prover would incorporate this data during proof construction.
    ///
    /// Generates a mock proof of state transition.
    ///
    /// # Parameters:
    /// - `nmt_comm`: NMT commitment for the block.
    /// - `state_commitment`: The new state commitment after transactions are applied.
    /// - `previous_state_commitment`: The state commitment before transactions are applied.
    /// - `namespace_proof`: Proof that transactions are included in the rollup's namespace.
    /// - `rollup_vm`: A reference to the RollupVM containing the VM ID.
    ///
    /// # Returns:
    /// - A `Proof` struct representing the transition.
    pub fn generate(
        nmt_comm: NMTRoot,
        state_commitment: Commitment<State>,
        previous_state_commitment: Commitment<State>,
        namespace_proof: NamespaceProofType,
        rollup_vm: &RollupVM,
    ) -> Self {
        // Verifies that the namespace proof matches the NMT root and the VM ID.
        namespace_proof
            .verify(&nmt_comm.root(), rollup_vm.id())
            .expect("Namespace proof failure, cannot continue")
            .expect("Namespace proof failure, cannot continue");
        // Creates and returns a mock proof.
        Self {
            block: nmt_comm.commit(),
            old_state: previous_state_commitment,
            new_state: state_commitment,
        }
    }
}

/// A mock proof aggregating a batch of proofs for a range of blocks.
#[derive(Debug, Clone, Into)]
pub(crate) struct BatchProof {
    first_block: Commitment<NMTRoot>,
    last_block: Commitment<NMTRoot>,
    old_state: Commitment<State>,
    new_state: Commitment<State>,
}

impl BatchProof {
    /// Generate a proof of correct execution of a range of blocks.
    /// # Parameters:
    /// - `proofs`: A list of individual proofs.
    ///
    /// # Returns:
    /// - A `BatchProof` struct representing the aggregate proof.
    ///
    /// # Error
    /// - Returns `ProofError::OutOfOrder` if proofs are not provided in consecutive order.

    pub fn generate(proofs: &[Proof]) -> Result<BatchProof, ProofError> {
        for i in 0..proofs.len() - 1 {
            if proofs[i].new_state != proofs[i + 1].old_state {
                return Err(ProofError::OutOfOrder {
                    position: i,
                    new_state: proofs[i].new_state,
                    old_state: proofs[i].old_state,
                });
            }
        }
        // Returns a new BatchProof if all proofs are in order.
        Ok(BatchProof {
            first_block: proofs[0].block,
            last_block: proofs[proofs.len() - 1].block,
            old_state: proofs[0].old_state,
            new_state: proofs[proofs.len() - 1].new_state,
        })
    }
}

/// Implements conversion from a contract-binding BatchProof to this internal BatchProof struct.
impl TryFrom<bindings::BatchProof> for BatchProof {
    type Error = SerializationError;

    fn try_from(p: bindings::BatchProof) -> Result<Self, Self::Error> {
        // Converts each field from U256 format to Commitment.
        Ok(Self {
            first_block: u256_to_commitment(p.first_block)?,
            last_block: u256_to_commitment(p.last_block)?,
            old_state: u256_to_commitment(p.old_state)?,
            new_state: u256_to_commitment(p.new_state)?,
        })
    }
}

/// Implements conversion from this internal BatchProof struct to a contract-binding BatchProof.
impl From<BatchProof> for bindings::BatchProof {
    fn from(p: BatchProof) -> Self {
        Self {
            first_block: commitment_to_u256(p.first_block),
            last_block: commitment_to_u256(p.last_block),
            old_state: commitment_to_u256(p.old_state),
            new_state: commitment_to_u256(p.new_state),
        }
    }
}
