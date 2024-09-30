// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

use std::time::Duration;

use crate::state::State;
use commit::Commitment;
use contract_bindings::example_rollup::ExampleRollup;
use ethers::{prelude::*, providers::Provider};
use sequencer_utils::{commitment_to_u256, test_utils::TestL1System, Signer};
use surf_disco::Url;

pub type ExampleRollupContract = ExampleRollup<Signer>;

/// Deploys the ExampleRollup smart contract on the Layer 1 test system.
///
/// This function uses the provided test Layer 1 system (TestL1System) to deploy the ExampleRollup contract.
/// It accepts the `initial_state` as a commitment to the `State` of the rollup and deploys the contract
/// using the deployer client in the test system.
///
/// Arguments:
/// - `test_l1`: A reference to the Layer 1 test system that provides necessary components like the deployer and hotshot address.
/// - `initial_state`: The initial commitment to the rollup state, converted into a `u256` type.
///
/// Returns:
/// - `ExampleRollupContract`: The contract instance for interacting with the deployed ExampleRollup contract.
pub async fn deploy_example_contract(
    test_l1: &TestL1System,
    initial_state: Commitment<State>,
) -> ExampleRollupContract {
    ExampleRollup::deploy(
        test_l1.clients.deployer.provider.clone(),
        (test_l1.hotshot.address(), commitment_to_u256(initial_state)),
    )
    .unwrap()
    .send()
    .await
    .unwrap()
}

/// Creates a provider for interacting with the blockchain using an HTTP URL.
///
/// This function sets up a provider (using the `ethers` library) that allows communication with an Ethereum node
/// or a Layer 1 test system via HTTP. It also sets the polling interval for provider operations.
///
/// Arguments:
/// - `l1_url`: The URL of the Layer 1 blockchain node or system.
///
/// Returns:
/// - `Provider<Http>`: The initialized provider for interacting with the blockchain.
pub fn create_provider(l1_url: &Url) -> Provider<Http> {
    let mut provider = Provider::try_from(l1_url.to_string()).unwrap();
    provider.set_interval(Duration::from_millis(10));
    provider
}
