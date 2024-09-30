// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the sequencer-example-l2 repository.

// You should have received a copy of the MIT License
// along with the sequencer-example-l2 repository. If not, see <https://mit-license.org/>.

use async_std::sync::RwLock;
use ethers::abi::Address;
use futures::FutureExt;
use sequencer::Transaction;
use sequencer::{Vm, VmTransaction};
use std::io;
use std::sync::Arc;
use surf_disco::{error::ClientError, Url};
use tide_disco::{error::ServerError, Api, App};

use crate::RollupVM;
use crate::{state::State, transaction::SignedTransaction};

#[derive(Clone, Debug)]

/*
2 options for users to submit their transactions.
- through rollup api
- through hotshot sequencer node api
*/
pub struct APIOptions {
    pub api_port: u16,
    pub sequencer_url: Url,
}

/*
Submits a signed transaction to a Rollup or Sequencer API.

 # Parameters
 - `submit_url`: The API endpoint URL.
 - `transaction`: The signed transaction to be encoded and submitted.
 - `vm`: A reference to the RollupVM providing the VM ID.

 # Returns
 - `Ok(())` if successful.
 - `Err(ServerError)` if any error occurs (e.g., during encoding or sending the request).
*/
async fn submit_transaction(
    submit_url: Url,
    transaction: SignedTransaction,
    vm: &RollupVM,
) -> Result<(), ServerError> {
    let raw_tx = transaction.encode();
    let txn = Transaction::new(vm.id(), raw_tx.to_vec());
    let client = surf_disco::Client::<ClientError>::new(submit_url);
    client
        .post::<()>("submit/submit")
        .body_json(&txn)?
        .send()
        .await?;
    Ok(())
}

/*
Serves an API for interacting with a rollup system, providing transaction submission,
 balance checking, and nonce retrieval functionalities.

 # Parameters
 - `options`: API configuration options (port, sequencer URL, etc.).
 - `state`: Shared application state containing blockchain data, wrapped in `Arc<RwLock<State>>`.

 # Behavior
 - Initializes the API using configuration from a `TOML` file.
 - Defines the following endpoints:
   - `POST /submit`: Submits a signed transaction to the sequencer.
   - `GET /balance`: Retrieves the balance for a specified Ethereum address.
   - `GET /nonce`: Retrieves the nonce for a specified Ethereum address.
 - Maps common errors (e.g., invalid addresses, malformed transactions) to appropriate HTTP error responses.
 - Starts serving the API at the specified `api_port`.

 # Errors
 - Returns `io::Result<()>`, where errors can arise from API setup, malformed requests,
   or transaction submission.

*/
pub async fn serve(options: &APIOptions, state: Arc<RwLock<State>>) -> io::Result<()> {
    type StateType = Arc<RwLock<State>>;
    let error_mapper = |err| io::Error::new(io::ErrorKind::Other, err);
    let APIOptions {
        api_port,
        sequencer_url,
        ..
    } = options.clone();
    let mut app = App::<StateType, ServerError>::with_state(state);
    let toml = toml::from_str::<toml::Value>(include_str!("api.toml"))
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    let mut api = Api::<StateType, ServerError>::new(toml).map_err(error_mapper)?;

    api.post("submit",  move|req, state| {
        let url = sequencer_url.clone();
        async move {
            let transaction = req
                .body_auto::<SignedTransaction>().
            map_err(|_| ServerError {
                status: tide_disco::StatusCode::BadRequest,
                message: "Malformed transaction. Ensure that the transaction is a JSON serialized SignedTransaction".into()
            })?;
            submit_transaction(url, transaction, &state.vm).await
        }
        .boxed()
    })
    .map_err(error_mapper)?;

    api.get("balance", |req, state| {
        async move {
            let address_str = req.string_param("address")?;
            let address = address_str.parse::<Address>().
            map_err(|_| ServerError {
                status: tide_disco::StatusCode::BadRequest,
                message: "Malformed address. Ensure that the address is valid hex encoded Ethereum address.".into()
            })?;
            let balance = state.get_balance(&address);
            Ok(balance)
        }
        .boxed()
    })
    .map_err(error_mapper)?;

    api.get("nonce", |req, state| {
        async move {
            let address_str = req.string_param("address")?;
            let address = address_str.parse::<Address>().
            map_err(|_| ServerError {
                status: tide_disco::StatusCode::BadRequest,
                message: "Malformed address. Ensure that the address is valid hex encoded Ethereum address.".into()
            })?;
            let nonce = state.get_nonce(&address);
            Ok(nonce)
        }
        .boxed()
    })
    .map_err(error_mapper)?;

    app.register_module("rollup", api)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    app.serve(format!("0.0.0.0:{}", api_port)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::Transaction;
    use async_std::task::spawn;
    use ethers::signers::{LocalWallet, Signer};
    use futures::future::ready;
    use portpicker::pick_unused_port;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use sequencer::{
        api::options::{Http, Options},
        context::SequencerContext,
        persistence::fs,
        testing::wait_for_decide_on_handle,
        Transaction as SeqTransaction,
    };
    use surf_disco::Client;
    use tempfile::TempDir;

    const GENESIS_BALANCE: u64 = 9999;

    #[async_std::test]
    async fn query_test() {
        let mut rng = rand::thread_rng();
        let genesis_wallet = LocalWallet::new(&mut rng);
        let vm = RollupVM::new(1.into());
        let genesis_address = genesis_wallet.address();
        let state = Arc::new(RwLock::new(State::from_initial_balances(
            [(genesis_address, GENESIS_BALANCE)],
            vm,
        )));
        let port = pick_unused_port().expect("No ports free");
        let api_url: Url = format!("http://localhost:{port}").parse().unwrap();
        let client: Client<ServerError> = Client::new(api_url.clone());
        let options = APIOptions {
            api_port: port,
            sequencer_url: api_url,
        };

        spawn(async move { serve(&options, state).await });

        client.connect(None).await;

        // Fetch genesis block balance
        let balance = client
            .get::<u64>(&format!("rollup/balance/{:?}", genesis_address))
            .send()
            .await
            .unwrap();

        assert_eq!(balance, GENESIS_BALANCE);
    }

    #[async_std::test]
    async fn submit_test() {
        // Start a sequencer network.
        let sequencer_port = pick_unused_port().unwrap();
        let vm = RollupVM::new(1.into());
        let nodes = sequencer::testing::init_hotshot_handles().await;
        let mut api_node = nodes[0].clone();
        let mut events = api_node.get_event_stream(Default::default()).await.0;
        let tmp_dir = TempDir::new().unwrap();
        let storage_path = tmp_dir.path().join("tmp_storage");
        let init_handle = Box::new(move |_| {
            ready(SequencerContext::new(
                api_node,
                0,
                Default::default(),
                Default::default(),
                None,
            ))
            .boxed()
        });
        Options::from(Http {
            port: sequencer_port,
        })
        .submit(Default::default())
        .query_fs(Default::default(), fs::Options { path: storage_path })
        .serve(init_handle)
        .await
        .unwrap();
        for node in &nodes {
            node.hotshot.start_consensus().await;
        }

        // Start the Rollup API
        let api_port = pick_unused_port().unwrap();
        let sequencer_url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();
        let genesis_wallet = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let genesis_address = genesis_wallet.address();
        let state = Arc::new(RwLock::new(State::from_initial_balances(
            [(genesis_address, GENESIS_BALANCE)],
            vm,
        )));
        let options = APIOptions {
            api_port,
            sequencer_url,
        };
        spawn(async move { serve(&options, state).await });

        // Create a transaction
        let transaction = Transaction {
            amount: 100,
            destination: genesis_address,
            nonce: 1,
        };
        let signed_transaction = SignedTransaction::new(transaction, &genesis_wallet).await;

        // Submit the transaction
        let api_url = format!("http://localhost:{api_port}").parse().unwrap();
        let api_client: Client<ServerError> = Client::new(api_url);
        api_client.connect(None).await;
        api_client
            .post::<()>("rollup/submit")
            .body_json(&signed_transaction)
            .unwrap()
            .send()
            .await
            .unwrap();

        // Wait for a Decide event containing transaction matching the one we sent
        let raw_tx = signed_transaction.encode();
        let txn = SeqTransaction::new(vm.id(), raw_tx.to_vec());
        wait_for_decide_on_handle(&mut events, &txn).await.unwrap()
    }
}
