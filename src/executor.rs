// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

use crate::prover::BatchProof;
use async_compatibility_layer::async_primitives::broadcast::BroadcastSender;
use async_std::sync::{Arc, RwLock};
use async_std::task::sleep;
use commit::Committable;
use contract_bindings::example_rollup::{self, ExampleRollup};
use ethers::prelude::*;
use hotshot_contract_bindings::hot_shot::{HotShot, NewBlocksFilter};
use sequencer::{api::endpoints::NamespaceProofQueryData, Header, Vm};
use surf_disco::Url;

use sequencer_utils::{commitment_to_u256, connect_rpc, contract_send, u256_to_commitment};

use crate::state::State;

type HotShotClient = surf_disco::Client<hotshot_query_service::Error>;

#[derive(Clone, Debug)]
pub struct ExecutorOptions {
    pub sequencer_url: Url,
    pub l1_http_provider: Url,
    pub l1_ws_provider: Url,
    pub rollup_account_index: u32,
    pub rollup_mnemonic: String,
    pub hotshot_address: Address,
    pub rollup_address: Address,
    pub output_stream: Option<BroadcastSender<(u64, State)>>,
}

/// Runs the executor service, which is responsible for:
/// 1) Fetching blocks of ordered transactions from HotShot and applying them to the Rollup State.
/// 2) Submitting mock proofs to the Rollup Contract.
pub async fn run_executor(opt: &ExecutorOptions, state: Arc<RwLock<State>>) {
    let ExecutorOptions {
        rollup_account_index,
        sequencer_url,
        l1_http_provider,
        l1_ws_provider,
        hotshot_address,
        rollup_address,
        rollup_mnemonic,
        output_stream,
    } = opt;

    // Build the URL to query the availability of blocks from HotShot
    let query_service_url = sequencer_url.join("availability").unwrap();

    let hotshot = HotShotClient::new(query_service_url.clone());
    hotshot.connect(None).await;

    // Connect to the layer one HotShot contract.
    let l1 = connect_rpc(
        l1_http_provider,
        rollup_mnemonic,
        *rollup_account_index,
        None,
    )
    .await
    .expect("unable to connect to L1, hotshot commitment task exiting");

    // Create a socket connection to the L1 to subscribe to contract events
    // This assumes that the L1 node supports both HTTP and Websocket connections
    let socket_provider = Provider::<Ws>::connect(l1_ws_provider)
        .await
        .expect("Unable to make websocket connection to L1");

    // Initialize the Rollup and HotShot contracts
    let rollup_contract = ExampleRollup::new(*rollup_address, Arc::new(l1));
    let hotshot_contract = HotShot::new(*hotshot_address, Arc::new(socket_provider));

    // Create a filter to listen to new block events from HotShot
    let filter = hotshot_contract
        .new_blocks_filter()
        .from_block(0)
        // Ethers does not set the contract address on filters created via contract bindings. This
        // seems like a bug and I have reported it: https://github.com/gakonst/ethers-rs/issues/2528.
        // In the mean time we can work around by setting the address manually.
        .address(hotshot_contract.address().into());

    // Subscribe to the block events stream
    let mut commits_stream = filter
        .subscribe()
        .await
        .expect("Unable to subscribe to L1 log stream");

    // Subscribe to the HotShot block header stream
    let mut header_stream = hotshot
        .socket("stream/headers/0")
        .subscribe::<Header>()
        .await
        .expect("Unable to subscribe to HotShot block header stream");

    // Get the VM ID of the Rollup
    let vm_id: u64 = state.read().await.vm.id().into();

    // Main loop: process each new block event
    while let Some(event) = commits_stream.next().await {
        // Extract block number and number of blocks from the event
        let (first_block, num_blocks) = match event {
            Ok(NewBlocksFilter {
                first_block_number,
                num_blocks,
            }) => (first_block_number, num_blocks.as_u64()),
            Err(err) => {
                tracing::error!("Error in HotShot block stream, retrying: {err}");
                continue;
            }
        };

        // Full block content may not be available immediately so wait for all blocks to be ready
        // before building the batch proof

        // Collect the block headers corresponding to the number of blocks received
        let headers: Vec<Header> = header_stream
            .by_ref()
            .take(num_blocks as usize)
            .map(|result| result.expect("Error fetching block header"))
            .collect()
            .await;

        // Execute new blocks, generating proofs.
        let mut proofs = vec![];
        tracing::info!(
            "executing blocks {}-{}, state is {}",
            first_block,
            first_block + num_blocks - 1,
            state.read().await.commit()
        );
        // Process each block in the batch, applying transactions to the rollup state
        for (i, header) in headers.into_iter().enumerate() {
            // Fetch the commitment from the HotShot contract for the block
            let commitment = hotshot_contract
                .commitments(first_block + i)
                .call()
                .await
                .expect("Unable to read commitment");

            // Deserialize the commitment into a usable format
            let block_commitment =
                u256_to_commitment(commitment).expect("Unable to deserialize block commitment");

            // Verify that the block commitment matches the hash of the received block
            if header.commit() != block_commitment {
                panic!("Block commitment does not match hash of received block, the executor cannot continue");
            }
            // Fetch the namespace proof for the transactions within the block
            let namespace_proof_query: NamespaceProofQueryData = hotshot
                .get(&format!(
                    "block/{}/namespace/{}",
                    first_block.as_u64() + (i as u64),
                    vm_id
                ))
                .send()
                .await
                .unwrap();
            let namespace_proof = namespace_proof_query.proof;

            // Apply the block's transactions to the current rollup state
            let mut state = state.write().await;
            proofs.push(
                state
                    .execute_block(header.transactions_root, namespace_proof)
                    .await,
            );

            // Optionally send the updated state through an output stream for other services
            if let Some(stream) = &output_stream {
                stream
                    .send_async((first_block.as_u64() + (i as u64), state.clone()))
                    .await
                    .ok();
            }
        }

        // Compute an aggregate proof.
        let proof = BatchProof::generate(&proofs).expect("Error generating batch proof");
        let state_comm = commitment_to_u256(state.read().await.commit());

        // Send the batch proof to L1.
        tracing::info!(
            "rollup {vm_id} sending batch proof of state {} after blocks {}-{} to L1: {:?}",
            state_comm,
            first_block,
            first_block + num_blocks - 1,
            proof,
        );

        // Convert the BatchProof into a format understood by the L1 Rollup Contract
        let proof = example_rollup::BatchProof::from(proof);

        // Attempt to send the batch proof to the Rollup Contract on L1
        let call = rollup_contract.verify_blocks(num_blocks, state_comm, proof);
        // Retry sending the proof if there is a failure, with a delay
        while let Err(err) = contract_send(&call).await {
            tracing::warn!("Failed to submit proof to contract, retrying: {err}");
            sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod test {
    use crate::state::{Amount, Nonce};
    use crate::transaction::{SignedTransaction, Transaction};
    use crate::utils::{create_provider, deploy_example_contract, ExampleRollupContract};
    use crate::RollupVM;

    use super::*;
    use async_compatibility_layer::{
        async_primitives::broadcast,
        logging::{setup_backtrace, setup_logging},
    };
    use async_std::task::spawn;
    use contract_bindings::example_rollup::StateUpdateFilter;
    use derivative::Derivative;
    use ethers::prelude::k256::ecdsa::SigningKey;
    use ethers::providers::{Middleware, Provider};
    use ethers::signers::{LocalWallet, Signer};
    use futures::{
        future::{join_all, ready},
        stream, FutureExt, Stream,
    };
    use hotshot::types::SystemContextHandle;
    use portpicker::pick_unused_port;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use sequencer::{
        api::options::{Http, Options},
        context::SequencerContext,
        hotshot_commitment::{run_hotshot_commitment_task, CommitmentTaskOptions},
        network,
        persistence::fs,
        testing::{init_hotshot_handles, wait_for_decide_on_handle},
        Node, SeqTypes, Vm, VmId,
    };
    use sequencer_utils::{commitment_to_u256, test_utils::TestL1System, Anvil, AnvilOptions};
    use std::path::PathBuf;
    use std::time::Duration;
    use surf_disco::{Client, Url};
    use tempfile::TempDir;
    use tide_disco::error::ServerError;

    #[derive(Clone, Derivative)]
    #[derivative(Debug)]
    struct TestRollupInstance {
        contract: ExampleRollupContract,
        vm: RollupVM,
        socket_provider: Provider<Ws>,
        l1_url: Url,
        alice: Wallet<SigningKey>,
        state: Arc<RwLock<State>>,
        bob: Wallet<SigningKey>,
        #[derivative(Debug = "ignore")]
        executor_send: BroadcastSender<(u64, State)>,
    }

    impl TestRollupInstance {
        pub async fn launch(
            l1_url: Url,
            vm_id: VmId,
            alice: Wallet<SigningKey>,
            bob: Wallet<SigningKey>,
            test_l1: &TestL1System,
        ) -> Self {
            // Create mock rollup state
            let vm = RollupVM::new(vm_id);
            let state = State::from_initial_balances([(alice.address(), 9999)], vm);
            let initial_state = state.commit();
            let state = Arc::new(RwLock::new(state));
            tracing::info!(
                "rollup {vm_id:?} initial state: {initial_state} ({})",
                commitment_to_u256(initial_state)
            );
            let mut ws_url = l1_url.clone();
            ws_url.set_scheme("ws").unwrap();
            let socket_provider = Provider::<Ws>::connect(ws_url).await.unwrap();
            let rollup_contract = deploy_example_contract(test_l1, initial_state).await;
            let (executor_send, _) = broadcast::channel();

            Self {
                contract: rollup_contract,
                vm,
                socket_provider,
                alice,
                l1_url,
                bob,
                state,
                executor_send,
            }
        }

        pub async fn reset_socket_connnection(&mut self) {
            let mut ws_url = self.l1_url.clone();
            ws_url.set_scheme("ws").unwrap();
            // Occasionally the connection fails, so we retry a few times.
            for _ in 0..10 {
                match Provider::<Ws>::connect(ws_url.clone()).await {
                    Ok(provider) => {
                        self.socket_provider = provider;
                        return;
                    }
                    Err(_) => {
                        tracing::warn!("Failed to connect to websocket, retrying");
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            panic!("Failed to connect to websocket server: {:?}", ws_url);
        }

        pub async fn subscribe_contract(
            &self,
        ) -> impl '_ + Stream<Item = (StateUpdateFilter, LogMeta)> {
            let filter = self
                .contract
                .state_update_filter()
                .filter
                // Ethers does not set the contract address on filters created via contract
                // bindings. This seems like a bug and I have reported it:
                // https://github.com/gakonst/ethers-rs/issues/2528. In the mean time we can work
                // around by setting the address manually.
                .address(self.contract.address());
            self.socket_provider
                .subscribe_logs(&filter)
                .await
                .unwrap()
                .map(|log| {
                    let meta = LogMeta::from(&log);
                    (parse_log(log).unwrap(), meta)
                })
        }

        pub async fn subscribe_executor(&self) -> impl Stream<Item = (u64, State)> {
            let recv = self.executor_send.handle_async().await;
            stream::unfold(recv, |mut recv| async move {
                Some((recv.recv_async().await.unwrap(), recv))
            })
            .boxed()
        }

        /// Wait until some effect has happened, causing the rollup state to satisfy `predicate`.
        ///
        /// At each intermediate state, including the terminal state, this function checks that the
        /// state reported by the executor matches the state in the contract. This implies that, if
        /// this function returns, `predicate` holds on both the executor state and some state that
        /// has been verified by the smart contract.
        pub async fn wait_for_effect(&self, predicate: impl Fn(State) -> bool) {
            let vm_id: VmId = self.vm.into();
            let mut exec_stream = self.subscribe_executor().await;
            let mut l1_stream = self.subscribe_contract().await;
            loop {
                // Get the next event from the contract.
                let (event, log) = l1_stream.next().await.unwrap();
                tracing::info!("rollup {vm_id:?} got contract event {event:?} {log:?}");

                // Advance the executor stream to the corresponding state.
                let state = loop {
                    let (block_index, state) = exec_stream.next().await.unwrap();
                    tracing::info!(
                        "rollup {vm_id:?} executor commitment after block {block_index} is {}",
                        commitment_to_u256(state.commit())
                    );
                    if block_index + 1 == event.block_height.as_u64() {
                        break state;
                    }
                };

                // Ensure the executor's state commitment matches the contract.
                let contract_comm = self
                    .contract
                    .state_commitment()
                    .block(log.block_number)
                    .call()
                    .await
                    .unwrap();
                tracing::info!(
                    "rollup {vm_id:?} contract commitment at block {} is {contract_comm}",
                    log.block_number
                );
                assert_eq!(commitment_to_u256(state.commit()), contract_comm);

                // If the predicate is satisfied, finish up.
                if predicate(state) {
                    break;
                }
            }
        }

        pub async fn test_transaction(
            &self,
            amount: Amount,
            nonce: Nonce,
        ) -> sequencer::Transaction {
            let txn = Transaction {
                amount,
                destination: self.bob.address(),
                nonce,
            };
            let txn = SignedTransaction::new(txn, &self.alice).await;
            self.vm.wrap(&txn)
        }
    }

    async fn start_query_service<N: network::Type>(
        port: u16,
        storage_path: PathBuf,
        node: SystemContextHandle<SeqTypes, Node<N>>,
    ) {
        let init_handle = Box::new(move |_| {
            ready(SequencerContext::new(
                node,
                0,
                Default::default(),
                Default::default(),
                None,
            ))
            .boxed()
        });
        Options::from(Http { port })
            .submit(Default::default())
            .status(Default::default())
            .query_fs(Default::default(), fs::Options { path: storage_path })
            .serve(init_handle)
            .await
            .unwrap();
    }

    async fn spawn_anvil() -> Anvil {
        let anvil = AnvilOptions::default()
            .block_time(Duration::from_secs(1))
            .spawn()
            .await;

        // When we are running a local Anvil node, as in tests, some endpoints (e.g. eth_feeHistory)
        // do not work until at least one block has been mined. Wait until the fee history endpoint
        // works.
        let provider = create_provider(&anvil.url());
        while let Err(err) = provider.fee_history(1, BlockNumber::Latest, &[]).await {
            tracing::warn!("RPC is not ready: {err}");
            sleep(Duration::from_secs(1)).await;
        }

        anvil
    }

    const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";
    #[async_std::test]
    async fn test_execute() {
        setup_logging();
        setup_backtrace();

        let anvil = spawn_anvil().await;
        let alice = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let bob = LocalWallet::new(&mut ChaChaRng::seed_from_u64(1));

        // Deploy hotshot contract
        let provider = create_provider(&anvil.url());
        let test_l1 = TestL1System::deploy(provider).await.unwrap();

        // Start a test Rollup instance
        let test_rollup =
            TestRollupInstance::launch(anvil.url().clone(), 10.into(), alice, bob, &test_l1).await;

        // Start a test HotShot configuration
        let sequencer_port = pick_unused_port().unwrap();
        let nodes = init_hotshot_handles().await;
        let api_node = nodes[0].clone();
        let tmp_dir = TempDir::new().unwrap();
        let storage_path = tmp_dir.path().join("tmp_storage");
        start_query_service(sequencer_port, storage_path, api_node).await;
        for node in &nodes {
            node.hotshot.start_consensus().await;
        }
        let sequencer_url: Url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();

        // Submit transaction to sequencer
        let client: Client<ServerError> = Client::new(sequencer_url.clone());
        let txn = test_rollup.test_transaction(100, 1).await;
        client.connect(None).await;
        client
            .post::<()>("submit/submit")
            .body_json(&txn)
            .unwrap()
            .send()
            .await
            .unwrap();

        // Spawn hotshot commitment and executor tasks
        let hotshot_opt = CommitmentTaskOptions {
            l1_provider: anvil.url(),
            sequencer_mnemonic: TEST_MNEMONIC.to_string(),
            sequencer_account_index: test_l1.clients.funded[0].index,
            hotshot_address: test_l1.hotshot.address(),
            l1_chain_id: None,
            query_service_url: Some(sequencer_url.clone()),
            delay: None,
        };

        let rollup_opt = ExecutorOptions {
            sequencer_url,
            rollup_account_index: test_l1.clients.funded[1].index,
            l1_http_provider: anvil.url(),
            l1_ws_provider: anvil.ws_url(),
            rollup_mnemonic: TEST_MNEMONIC.to_string(),
            hotshot_address: test_l1.hotshot.address(),
            rollup_address: test_rollup.contract.address(),
            output_stream: Some(test_rollup.executor_send.clone()),
        };

        let state_lock = test_rollup.state.clone();
        spawn(async move { run_hotshot_commitment_task(&hotshot_opt).await });
        spawn(async move { run_executor(&rollup_opt, state_lock).await });

        // Wait for the rollup contract to process all state updates
        test_rollup
            .wait_for_effect(|state| {
                let bob_balance = state.get_balance(&test_rollup.bob.address());
                tracing::info!("Bob's balance is {bob_balance}/100");
                bob_balance == 100
            })
            .await;
    }

    #[async_std::test]
    async fn test_execute_multi_rollup() {
        setup_logging();
        setup_backtrace();

        let anvil = spawn_anvil().await;
        let alice = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let bob = LocalWallet::new(&mut ChaChaRng::seed_from_u64(1));
        // Deploy hotshot contract
        let provider = create_provider(&anvil.url());
        let test_l1 = TestL1System::deploy(provider).await.unwrap();

        // Start test Rollup instances
        let num_rollups = 3;
        let mut test_rollups = Vec::new();
        for i in 1..num_rollups + 1 {
            // To keep nonces consistent for the underlying provider, we must await these iteratively
            let test_rollup = TestRollupInstance::launch(
                anvil.url().clone(),
                i.into(),
                alice.clone(),
                bob.clone(),
                &test_l1,
            )
            .await;
            test_rollups.push(test_rollup);
        }

        // Start a test HotShot configuration
        let sequencer_port = pick_unused_port().unwrap();
        let nodes = init_hotshot_handles().await;
        let api_node = nodes[0].clone();
        let tmp_dir = TempDir::new().unwrap();
        let storage_path = tmp_dir.path().join("tmp_storage");
        start_query_service(sequencer_port, storage_path, api_node).await;
        for node in &nodes {
            node.hotshot.start_consensus().await;
        }
        let sequencer_url: Url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();

        // Submit transactions to sequencer
        let client: Client<ServerError> = Client::new(sequencer_url.clone());
        for i in 0..num_rollups {
            let txn = test_rollups[i as usize].test_transaction(100, 1).await;
            client.connect(None).await;
            client
                .post::<()>("submit/submit")
                .body_json(&txn)
                .unwrap()
                .send()
                .await
                .unwrap();
        }

        // Spawn hotshot commitment task
        let hotshot_opt = CommitmentTaskOptions {
            l1_provider: anvil.url(),
            sequencer_mnemonic: TEST_MNEMONIC.to_string(),
            sequencer_account_index: test_l1.clients.funded[0].index,
            hotshot_address: test_l1.hotshot.address(),
            l1_chain_id: None,
            query_service_url: Some(sequencer_url.clone()),
            delay: None,
        };
        spawn(async move { run_hotshot_commitment_task(&hotshot_opt).await });

        // Spawn all rollup executors
        for test_rollup in &test_rollups {
            let state_lock = test_rollup.state.clone();
            let rollup_opt = ExecutorOptions {
                sequencer_url: sequencer_url.clone(),
                rollup_account_index: test_l1.clients.funded[1].index,
                l1_http_provider: anvil.url(),
                l1_ws_provider: anvil.ws_url(),
                rollup_mnemonic: TEST_MNEMONIC.to_string(),
                hotshot_address: test_l1.hotshot.address(),
                rollup_address: test_rollup.contract.address(),
                output_stream: Some(test_rollup.executor_send.clone()),
            };
            spawn(async move { run_executor(&rollup_opt, state_lock).await });
        }

        // Wait for all rollup contracts to process state updates
        join_all(test_rollups.iter().map(|test_rollup| {
            test_rollup.wait_for_effect(|state| {
                let bob_balance = state.get_balance(&test_rollup.bob.address());
                tracing::info!("Bob's balance is {bob_balance}/100");
                bob_balance == 100
            })
        }))
        .await;
    }

    #[async_std::test]
    async fn test_execute_batched_updates_to_slow_l1() {
        setup_logging();
        setup_backtrace();

        let num_txns = 10;

        // Create mock rollup state
        let alice = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let bob = LocalWallet::new(&mut ChaChaRng::seed_from_u64(1));

        // Start a test HotShot and Rollup contract.
        let mut anvil = spawn_anvil().await;
        let provider = create_provider(&anvil.url());
        let test_l1 = TestL1System::deploy(provider).await.unwrap();
        let mut test_rollup =
            TestRollupInstance::launch(anvil.url().clone(), 20.into(), alice, bob, &test_l1).await;

        // Once the contracts have been deployed, restart the L1 with a slow block time.
        anvil
            .restart(AnvilOptions::default().block_time(Duration::from_secs(5)))
            .await;
        test_rollup.reset_socket_connnection().await;

        // Start a test HotShot configuration
        let sequencer_port = pick_unused_port().unwrap();
        let nodes = init_hotshot_handles().await;
        let mut api_node = nodes[0].clone();
        let mut events = api_node.get_event_stream(Default::default()).await.0;
        let tmp_dir = TempDir::new().unwrap();
        let storage_path = tmp_dir.path().join("tmp_storage");
        start_query_service(sequencer_port, storage_path, api_node).await;
        for node in &nodes {
            node.hotshot.start_consensus().await;
        }
        let sequencer_url: Url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();

        // Spawn hotshot commitment and executor tasks
        let hotshot_opt = CommitmentTaskOptions {
            l1_provider: anvil.url(),
            sequencer_mnemonic: TEST_MNEMONIC.to_string(),
            sequencer_account_index: test_l1.clients.funded[0].index,
            hotshot_address: test_l1.hotshot.address(),
            l1_chain_id: None,
            query_service_url: Some(sequencer_url.clone()),
            delay: None,
        };

        let rollup_opt = ExecutorOptions {
            sequencer_url,
            l1_http_provider: anvil.url(),
            l1_ws_provider: anvil.ws_url(),
            rollup_account_index: test_l1.clients.funded[1].index,
            rollup_mnemonic: TEST_MNEMONIC.to_string(),
            hotshot_address: test_l1.hotshot.address(),
            rollup_address: test_rollup.contract.address(),
            output_stream: Some(test_rollup.executor_send.clone()),
        };

        let state_lock = test_rollup.state.clone();
        spawn(async move { run_hotshot_commitment_task(&hotshot_opt).await });
        spawn(async move { run_executor(&rollup_opt, state_lock).await });

        // Submit transactions to sequencer
        for nonce in 1..=num_txns {
            let txn = test_rollup.test_transaction(1, nonce).await;
            nodes[0]
                .submit_transaction(txn.clone())
                .await
                .expect("Transaction submission should not fail");

            // Wait for the transaction to be sequenced, before we can sequence the next one.
            tracing::info!("Waiting for txn {nonce} to be sequenced");
            wait_for_decide_on_handle(&mut events, &txn).await.unwrap();
        }

        // Wait for the rollup contract to process all state updates
        test_rollup
            .wait_for_effect(|state| {
                let bob_balance = state.get_balance(&test_rollup.bob.address());
                tracing::info!("Bob's balance is {bob_balance}/{num_txns}");
                bob_balance == num_txns
            })
            .await;
    }
}
