// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.
//
// This file defines the main configuration options and basic structures
// needed to interact with the Rollup and HotShot sequencer system.

// External libraries and modules are imported here.
use clap::Parser;
use derive_more::{From, Into};
use ethers::types::Address;
use sequencer::{Vm, VmId};
use surf_disco::Url;
use transaction::SignedTransaction;

// Internal modules for various functionality in the system.
pub mod api;
pub mod error;
pub mod executor;
mod prover;
pub mod seed;
pub mod state;
pub mod transaction;
pub mod utils;

/// `Options` struct defines configuration parameters for the rollup system.
/// These parameters are provided via environment variables or command-line arguments.
#[derive(Parser, Clone, Debug)]
pub struct Options {
    /// Port where the Rollup API will be served.
    #[clap(short, long, env = "ESPRESSO_DEMO_ROLLUP_PORT", default_value = "8084")]
    pub api_port: u16,

    /// URL of a HotShot sequencer node for transaction submission.
    #[clap(
        long,
        env = "ESPRESSO_SEQUENCER_URL",
        default_value = "http://localhost:50000"
    )]
    pub sequencer_url: Url,

    /// URL of the Ethereum JSON-RPC provider for Layer 1 (HTTP).
    #[clap(
        long,
        env = "ESPRESSO_DEMO_L1_HTTP_PROVIDER",
        default_value = "http://localhost:8545"
    )]
    pub l1_http_provider: Url,

    /// WebSocket URL for the Layer 1 Ethereum provider (WebSocket).
    #[clap(
        long,
        env = "ESPRESSO_DEMO_L1_WS_PROVIDER",
        default_value = "ws://localhost:8545"
    )]
    pub l1_ws_provider: Url,

    /// Address of the HotShot contract deployed on Layer 1 Ethereum.
    #[clap(
        long,
        env = "ESPRESSO_DEMO_HOTSHOT_ADDRESS",
        default_value = "0x0116686e2291dbd5e317f47fadbfb43b599786ef"
    )]
    pub hotshot_address: Address,

    /// Mnemonic phrase used by the rollup wallet.
    /// This wallet will send proofs of transaction validity to the rollup contract and must be funded.
    #[clap(
        long,
        env = "ESPRESSO_DEMO_ROLLUP_MNEMONIC",
        default_value = "test test test test test test test test test test test junk"
    )]
    pub rollup_mnemonic: String,

    /// Index of the account derived from the mnemonic that will send proofs to the rollup contract.
    #[clap(long, env = "ESPRESSO_DEMO_ROLLUP_ACCOUNT_INDEX", default_value = "1")]
    pub rollup_account_index: u32,
}

/// `RollupVM` struct represents a virtual machine (VM) in the rollup system.
/// It wraps around a `VmId` to uniquely identify the VM.
#[derive(Clone, Copy, Debug, Default, Into, From)]
pub struct RollupVM(VmId);

/// Implementation of the `RollupVM` struct.
impl RollupVM {
    /// Constructor to create a new `RollupVM` with the given `VmId`.
    pub fn new(id: VmId) -> Self {
        RollupVM(id)
    }
}

/// Implementation of the `Vm` trait for `RollupVM`.
/// This allows `RollupVM` to function as a VM in the system with associated transactions.
impl Vm for RollupVM {
    type Transaction = SignedTransaction;

    /// Returns the VM's unique identifier (`VmId`).
    fn id(&self) -> VmId {
        self.0
    }
}
