// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
use async_std::sync::RwLock;
use clap::Parser;
use commit::Committable;
use ethers::signers::{LocalWallet, Signer};
use example_l2::{
    api::{serve, APIOptions},
    executor::{run_executor, ExecutorOptions},
    seed::{SeedIdentity, INITIAL_BALANCE},
    state::State,
    utils::{create_provider, deploy_example_contract},
    Options, RollupVM,
};
use futures::join;
use rand::SeedableRng;
use rand_chacha::ChaChaRng;
use sequencer_utils::test_utils::TestL1System;
use std::sync::Arc;
use strum::IntoEnumIterator;

#[async_std::main]
async fn main() {
    // Set up logging and backtrace for error reporting
    setup_logging();
    setup_backtrace();

    // Parse command-line arguments using the Options struct
    let opt = Options::parse();

    // Initialize the Rollup Virtual Machine (VM) with a unique ID (1)
    let vm = RollupVM::new(1.into());

    // Initialize a vector to store initial account balances
    let mut initial_balances = vec![];

    /*
    Generate initial account balances for predefined identities.

    This loop iterates over a list of identities (SeedIdentity),
    generating a unique wallet address for each identity using a
    cryptographically secure random number generator (ChaChaRng).
    The generated address and an associated initial balance are
    stored in the `initial_balances` vector.

    Each identity (e.g., Alice, Bob, Charlie) is deterministically
    linked to a wallet using a seed derived from its enum value.
    */
    for identity in SeedIdentity::iter() {
        let address = LocalWallet::new(&mut ChaChaRng::seed_from_u64(identity as u64)).address();
        initial_balances.push((address, INITIAL_BALANCE));
    }

    /*
    Initialize the rollup's state with the generated account balances.

    The `State` structure encapsulates the rollup's account state,
    which includes:

    - Accounts: A mapping of Ethereum addresses to account balances,
      stored in a `BTreeMap`.
    - NMT Commitment: A cryptographic commitment to the Namespace
      Merkle Tree (NMT), which tracks the most recent transaction state.
    - Previous State Commitment: A commitment to the rollup state
      prior to the last executed transaction, allowing for
      verifiable state transitions.
    - VM: Information about the Rollup VM, such as its ID.

    The state is protected by an `RwLock` to ensure thread-safe
    asynchronous access and shared using an `Arc`.
    */
    let state = Arc::new(RwLock::new(State::from_initial_balances(
        initial_balances,
        vm,
    )));

    /*
    Set up the API options for the rollup.

    `APIOptions` configures how the user interacts with the rollup,
    allowing them to submit transactions either via the rollup API
    or directly to the HotShot sequencer node API. These options
    include the API port and the URL of the sequencer node.
    */
    let api_options = APIOptions {
        api_port: opt.api_port,
        sequencer_url: opt.sequencer_url.clone(),
    };

    /*
    Start the API server to handle user transactions and queries.

    The `serve_api` async block starts a server that listens for
    transaction submissions and state queries. The server interacts
    with the shared rollup state to read or modify the state based
    on user input.
    */
    let serve_api = async {
        serve(&api_options, state.clone()).await.unwrap();
    };

    // Generate an initial state commitment, which is used for verifiable rollup state transitions.
    let initial_state = { state.read().await.commit() };

    // Log information about the contract deployment process
    tracing::info!("Deploying Rollup contracts");

    // Create an Ethereum provider that connects to the Layer 1 node via HTTP
    let provider = create_provider(&opt.l1_http_provider);

    // Initialize the test system, which interacts with the Layer 1 system, and deploy the rollup contract
    let test_system = TestL1System::new(provider, opt.hotshot_address)
        .await
        .unwrap();
    let rollup_contract = deploy_example_contract(&test_system, initial_state).await;

    // Configure options for the executor, which manages block execution on the rollup
    let executor_options = ExecutorOptions {
        hotshot_address: opt.hotshot_address,
        l1_http_provider: opt.l1_http_provider.clone(),
        l1_ws_provider: opt.l1_ws_provider.clone(),
        rollup_address: rollup_contract.address(),
        rollup_account_index: opt.rollup_account_index,
        rollup_mnemonic: opt.rollup_mnemonic.clone(),
        sequencer_url: opt.sequencer_url.clone(),
        output_stream: None,
    };

    tracing::info!("Launching Example Rollup API and Executor");

    /*
    Run the executor and API concurrently.

    The executor is responsible for:
    - Fetching ordered transaction blocks from the HotShot node and
      applying them to the rollup VM state.
    - Posting mock proofs to the rollup contract on the Layer 1 chain.

    Both the executor and API server are run concurrently using `join!`.
    */
    join!(run_executor(&executor_options, state.clone()), serve_api);
}
