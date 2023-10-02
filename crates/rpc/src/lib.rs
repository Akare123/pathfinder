//! Starknet node JSON-RPC related modules.
pub mod context;
mod error;
mod executor;
mod felt;
pub mod gas_price;
mod jsonrpc;
pub mod middleware;
mod pathfinder;
#[cfg(test)]
mod test_setup;
pub mod v02;
pub mod v03;
pub mod v04;

pub use executor::compose_executor_transaction;

use crate::jsonrpc::rpc_handler;
pub use crate::jsonrpc::websocket::{BlockHeader, WebsocketSenders};
use crate::v02::types::syncing::Syncing;
use anyhow::Context;
use axum::error_handling::HandleErrorLayer;
use axum::extract::DefaultBodyLimit;

use axum::response::IntoResponse;
use context::RpcContext;
use http::Request;
use hyper::Body;
use pathfinder_common::AllowedOrigins;
use std::num::NonZeroUsize;
use std::{net::SocketAddr, result::Result};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_http::cors::CorsLayer;

const DEFAULT_MAX_CONNECTIONS: usize = 1024;

pub enum DefaultVersion {
    V03,
    V04,
}

pub struct RpcServer {
    addr: SocketAddr,
    context: RpcContext,
    max_connections: usize,
    cors: Option<CorsLayer>,
    ws_senders: Option<WebsocketSenders>,
    default_version: DefaultVersion,
}

impl RpcServer {
    pub fn new(addr: SocketAddr, context: RpcContext, default_version: DefaultVersion) -> Self {
        Self {
            addr,
            context,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            cors: None,
            ws_senders: None,
            default_version,
        }
    }

    pub fn with_ws(self, capacity: NonZeroUsize) -> Self {
        Self {
            ws_senders: Some(WebsocketSenders::with_capacity(capacity.get())),
            ..self
        }
    }

    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    pub fn with_cors(self, allowed_origins: AllowedOrigins) -> Self {
        Self {
            cors: Some(middleware::cors::with_allowed_origins(allowed_origins)),
            ..self
        }
    }

    /// Starts the HTTP-RPC server.
    pub fn spawn(self) -> Result<(JoinHandle<anyhow::Result<()>>, SocketAddr), anyhow::Error> {
        use axum::routing::{get, post};

        // TODO: make this configurable
        const REQUEST_MAX_SIZE: usize = 10 * 1024 * 1024;
        // TODO: make this configurable
        const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

        let listener = match std::net::TcpListener::bind(self.addr) {
            Ok(listener) => listener,
            Err(e) => return Err(e).context(format!("RPC address {} is already in use.
    
            Hint: This usually means you are already running another instance of pathfinder.
            Hint: If this happens when upgrading, make sure to shut down the first one first.
            Hint: If you are looking to run two instances of pathfinder, you must configure them with different http rpc addresses.", self.addr)),
        };
        let addr = listener
            .local_addr()
            .context("Getting local address from listener")?;
        let server = axum::Server::from_tcp(listener).context("Binding server to tcp listener")?;

        async fn handle_middleware_errors(err: axum::BoxError) -> (http::StatusCode, String) {
            use http::StatusCode;
            if err.is::<tower::timeout::error::Elapsed>() {
                (
                    StatusCode::REQUEST_TIMEOUT,
                    "Request took too long".to_string(),
                )
            } else {
                // TODO: confirm this isn't too verbose.
                tracing::warn!(error = err, "Unhandled middleware error");

                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal error".to_string(),
                )
            }
        }

        let middleware = tower::ServiceBuilder::new()
            // Convert errors created by middleware layers into responses.
            // This is required by axum -- axum doesn't deal with Result, errors
            // must be responses as well.
            .layer(HandleErrorLayer::new(handle_middleware_errors))
            .concurrency_limit(self.max_connections)
            .layer(DefaultBodyLimit::max(REQUEST_MAX_SIZE))
            .timeout(REQUEST_TIMEOUT)
            .layer(tower_http::trace::TraceLayer::new_for_http())
            .option_layer(self.cors);

        /// Returns success for requests with an empty body without reading
        /// the entire body.
        async fn empty_body(request: Request<Body>) -> impl IntoResponse {
            use hyper::body::HttpBody;
            if request.body().is_end_stream() {
                http::StatusCode::OK.into_response()
            } else {
                http::StatusCode::METHOD_NOT_ALLOWED.into_response()
            }
        }

        let v03_routes = v03::register_routes().build(self.context.clone());
        let v04_routes = v04::register_routes().build(self.context.clone());
        let pathfinder_routes = pathfinder::register_routes().build(self.context.clone());

        let default_router = match self.default_version {
            DefaultVersion::V03 => v03_routes.clone(),
            DefaultVersion::V04 => v04_routes.clone(),
        };

        let router = axum::Router::new()
            // Also return success for get's with an empty body. These are often
            // used by monitoring bots to check service health.
            .route("/", get(empty_body).post(rpc_handler))
            .with_state(default_router)
            .route("/rpc/v0.3", post(rpc_handler))
            .with_state(v03_routes)
            .route("/rpc/v0.4", post(rpc_handler))
            .with_state(v04_routes)
            .route("/rpc/pathfinder/v0.1", post(rpc_handler))
            .with_state(pathfinder_routes)
            .layer(middleware);
        // TODO: websockets

        let server_handle = tokio::spawn(async move {
            server
                .serve(router.into_make_service())
                .await
                .map_err(Into::into)
        });

        Ok((server_handle, addr))
    }

    pub fn get_ws_senders(&self) -> WebsocketSenders {
        // For parts in code that require WebsocketSenders
        match &self.ws_senders {
            Some(txs) => txs.clone(),
            // Returns WebsocketSenders instance for code to work as is.
            // Nothing is actually done coz no one can subscribe.
            _ => WebsocketSenders::with_capacity(1),
        }
    }
}

pub struct SyncState {
    pub status: RwLock<Syncing>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            status: RwLock::new(Syncing::False(false)),
        }
    }
}

pub mod test_utils {
    use pathfinder_common::event::Event;
    use pathfinder_common::{macro_prelude::*, ContractStateHash};
    use pathfinder_common::{
        BlockHeader, BlockNumber, BlockTimestamp, ContractAddress, EntryPoint, EthereumAddress,
        GasPrice, SierraHash, StarknetVersion, StateUpdate, StorageCommitment, TransactionIndex,
        TransactionVersion,
    };
    use pathfinder_merkle_tree::StorageCommitmentTree;
    use pathfinder_storage::{BlockId, Storage};
    use primitive_types::{H160, H256};
    use stark_hash::Felt;
    use starknet_gateway_types::reply::transaction::{ExecutionStatus, L2ToL1Message};
    use starknet_gateway_types::{
        pending::PendingData,
        reply::transaction::{
            DeployTransaction, EntryPointType, ExecutionResources, InvokeTransaction,
            InvokeTransactionV0, Receipt, Transaction,
        },
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    // Creates storage for tests
    pub fn setup_storage() -> Storage {
        use pathfinder_merkle_tree::contract_state::update_contract_state;

        let storage = Storage::in_memory().unwrap();
        let mut connection = storage.connection().unwrap();
        let db_txn = connection.transaction().unwrap();

        let class_commitment0 = class_commitment_bytes!(b"class commitment 0");
        let class_commitment1 = class_commitment_bytes!(b"class commitment 1");
        let class_commitment2 = class_commitment_bytes!(b"class commitment 2");

        let contract0_addr = contract_address_bytes!(b"contract 0");
        let contract1_addr = contract_address_bytes!(b"contract 1");
        let contract2_addr = contract_address_bytes!(b"contract 2 (sierra)");

        let class0_hash = class_hash_bytes!(b"class 0 hash");
        let class1_hash = class_hash_bytes!(b"class 1 hash");
        let class2_hash = class_hash_bytes!(b"class 2 hash (sierra)");
        let class_hash_pending = class_hash_bytes!(b"class pending hash");

        let storage_addr = storage_address_bytes!(b"storage addr 0");

        let state_update0 = StateUpdate::default()
            .with_deployed_contract(contract0_addr, class0_hash)
            .with_contract_nonce(contract0_addr, contract_nonce!("0x1"));

        let state_update1 = StateUpdate::default()
            .with_deployed_contract(contract1_addr, class1_hash)
            .with_storage_update(
                contract1_addr,
                storage_addr,
                storage_value_bytes!(b"storage value 1"),
            );

        let state_update2 = StateUpdate::default()
            .with_deployed_contract(contract2_addr, class2_hash)
            .with_contract_nonce(contract1_addr, contract_nonce!("0x10"))
            .with_contract_nonce(contract2_addr, contract_nonce!("0xfeed"))
            .with_storage_update(
                contract1_addr,
                storage_addr,
                storage_value_bytes!(b"storage value 2"),
            );

        let contract0_update = HashMap::new();

        let storage_addr = storage_address_bytes!(b"storage addr 0");
        let contract1_update0 =
            HashMap::from([(storage_addr, storage_value_bytes!(b"storage value 0"))]);
        let contract1_update1 =
            HashMap::from([(storage_addr, storage_value_bytes!(b"storage value 1"))]);
        let contract1_update2 =
            HashMap::from([(storage_addr, storage_value_bytes!(b"storage value 2"))]);

        let class0_definition =
            starknet_gateway_test_fixtures::class_definitions::CONTRACT_DEFINITION.to_vec();
        let class1_definition = &class0_definition;
        let sierra_class_definition =
            starknet_gateway_test_fixtures::class_definitions::CAIRO_0_11_SIERRA.to_vec();

        db_txn
            .insert_cairo_class(class0_hash, &class0_definition)
            .unwrap();
        db_txn
            .insert_cairo_class(class1_hash, class1_definition)
            .unwrap();
        db_txn
            .insert_sierra_class(
                &SierraHash(class2_hash.0),
                &sierra_class_definition,
                &casm_hash_bytes!(b"non-existent"),
                &[],
                "compiler version 123",
            )
            .unwrap();
        db_txn
            .insert_cairo_class(class_hash_pending, &class0_definition)
            .unwrap();

        let mut storage_commitment_tree =
            StorageCommitmentTree::load(&db_txn, StorageCommitment(Felt::ZERO));
        let state_hash = storage_commitment_tree
            .get(contract0_addr)
            .unwrap()
            .unwrap_or(ContractStateHash(Felt::ZERO));
        let contract_update_result = update_contract_state(
            contract0_addr,
            &contract0_update,
            Some(contract_nonce!("0x1")),
            Some(class0_hash),
            state_hash,
            &db_txn,
            false,
        )
        .unwrap();
        storage_commitment_tree
            .set(contract0_addr, contract_update_result.state_hash)
            .unwrap();
        contract_update_result.insert(&db_txn).unwrap();

        let (storage_commitment0, nodes) = storage_commitment_tree.commit().unwrap();
        db_txn
            .insert_storage_trie(storage_commitment0, &nodes)
            .unwrap();

        let mut storage_commitment_tree = StorageCommitmentTree::load(&db_txn, storage_commitment0);
        let state_hash = storage_commitment_tree
            .get(contract1_addr)
            .unwrap()
            .unwrap_or(ContractStateHash(Felt::ZERO));
        let contract_update_result = update_contract_state(
            contract1_addr,
            &contract1_update0,
            None,
            Some(class1_hash),
            state_hash,
            &db_txn,
            false,
        )
        .unwrap();
        storage_commitment_tree
            .set(contract1_addr, contract_update_result.state_hash)
            .unwrap();
        contract_update_result.insert(&db_txn).unwrap();
        let state_hash = storage_commitment_tree
            .get(contract1_addr)
            .unwrap()
            .unwrap_or(ContractStateHash(Felt::ZERO));
        let contract_update_result = update_contract_state(
            contract1_addr,
            &contract1_update1,
            None,
            None,
            state_hash,
            &db_txn,
            false,
        )
        .unwrap();
        storage_commitment_tree
            .set(contract1_addr, contract_update_result.state_hash)
            .unwrap();
        contract_update_result.insert(&db_txn).unwrap();
        let (storage_commitment1, nodes) = storage_commitment_tree.commit().unwrap();
        db_txn
            .insert_storage_trie(storage_commitment1, &nodes)
            .unwrap();

        let mut storage_commitment_tree = StorageCommitmentTree::load(&db_txn, storage_commitment1);
        let state_hash = storage_commitment_tree
            .get(contract1_addr)
            .unwrap()
            .unwrap_or(ContractStateHash(Felt::ZERO));
        let contract_update_result = update_contract_state(
            contract1_addr,
            &contract1_update2,
            Some(contract_nonce!("0x10")),
            None,
            state_hash,
            &db_txn,
            false,
        )
        .unwrap();
        storage_commitment_tree
            .set(contract1_addr, contract_update_result.state_hash)
            .unwrap();
        contract_update_result.insert(&db_txn).unwrap();
        let state_hash = storage_commitment_tree
            .get(contract2_addr)
            .unwrap()
            .unwrap_or(ContractStateHash(Felt::ZERO));
        let contract_update_result = update_contract_state(
            contract2_addr,
            &HashMap::new(),
            Some(contract_nonce!("0xfeed")),
            Some(class2_hash),
            state_hash,
            &db_txn,
            false,
        )
        .unwrap();
        storage_commitment_tree
            .set(contract2_addr, contract_update_result.state_hash)
            .unwrap();
        contract_update_result.insert(&db_txn).unwrap();
        let (storage_commitment2, nodes) = storage_commitment_tree.commit().unwrap();
        db_txn
            .insert_storage_trie(storage_commitment2, &nodes)
            .unwrap();

        let header0 = BlockHeader::builder()
            .with_number(BlockNumber::GENESIS)
            .with_storage_commitment(storage_commitment0)
            .with_class_commitment(class_commitment0)
            .with_calculated_state_commitment()
            .finalize_with_hash(block_hash_bytes!(b"genesis"));
        let header1 = header0
            .child_builder()
            .with_timestamp(BlockTimestamp::new_or_panic(1))
            .with_storage_commitment(storage_commitment1)
            .with_class_commitment(class_commitment1)
            .with_calculated_state_commitment()
            .with_gas_price(GasPrice::from(1))
            .with_sequencer_address(sequencer_address_bytes!(&[1u8]))
            .finalize_with_hash(block_hash_bytes!(b"block 1"));
        let header2 = header1
            .child_builder()
            .with_timestamp(BlockTimestamp::new_or_panic(2))
            .with_storage_commitment(storage_commitment2)
            .with_class_commitment(class_commitment2)
            .with_calculated_state_commitment()
            .with_gas_price(GasPrice::from(2))
            .with_sequencer_address(sequencer_address_bytes!(&[2u8]))
            .finalize_with_hash(block_hash_bytes!(b"latest"));

        db_txn.insert_block_header(&header0).unwrap();
        db_txn.insert_block_header(&header1).unwrap();
        db_txn.insert_block_header(&header2).unwrap();

        let txn0_hash = transaction_hash_bytes!(b"txn 0");
        // TODO introduce other types of transactions too
        let txn0 = InvokeTransactionV0 {
            calldata: vec![],
            sender_address: contract0_addr,
            entry_point_type: Some(EntryPointType::External),
            entry_point_selector: EntryPoint(Felt::ZERO),
            max_fee: pathfinder_common::Fee::ZERO,
            signature: vec![],
            transaction_hash: txn0_hash,
        };
        let mut receipt0 = Receipt {
            actual_fee: None,
            events: vec![],
            execution_resources: Some(ExecutionResources {
                builtin_instance_counter: Default::default(),
                n_memory_holes: 0,
                n_steps: 0,
            }),
            l1_to_l2_consumed_message: None,
            l2_to_l1_messages: vec![],
            transaction_hash: txn0_hash,
            transaction_index: TransactionIndex::new_or_panic(0),
            execution_status: Default::default(),
            revert_error: Default::default(),
        };
        let txn1_hash = transaction_hash_bytes!(b"txn 1");
        let txn2_hash = transaction_hash_bytes!(b"txn 2");
        let txn3_hash = transaction_hash_bytes!(b"txn 3");
        let txn4_hash = transaction_hash_bytes!(b"txn 4 ");
        let txn5_hash = transaction_hash_bytes!(b"txn 5");
        let txn6_hash = transaction_hash_bytes!(b"txn 6");
        let txn_reverted_hash = transaction_hash_bytes!(b"txn reverted");

        let mut txn1 = txn0.clone();
        let mut txn2 = txn0.clone();
        let mut txn3 = txn0.clone();
        let mut txn4 = txn0.clone();
        let mut txn6 = txn0.clone();
        let mut txn_reverted = txn0.clone();
        txn1.transaction_hash = txn1_hash;
        txn1.sender_address = contract1_addr;
        txn2.transaction_hash = txn2_hash;
        txn2.sender_address = contract1_addr;
        txn3.transaction_hash = txn3_hash;
        txn3.sender_address = contract1_addr;
        txn4.transaction_hash = txn4_hash;
        txn6.sender_address = contract1_addr;
        txn6.transaction_hash = txn6_hash;
        txn_reverted.transaction_hash = txn_reverted_hash;

        txn4.sender_address = ContractAddress::new_or_panic(Felt::ZERO);
        let mut txn5 = txn4.clone();
        txn5.transaction_hash = txn5_hash;
        let txn0 = Transaction::Invoke(txn0.into());
        let txn1 = Transaction::Invoke(txn1.into());
        let txn2 = Transaction::Invoke(txn2.into());
        let txn3 = Transaction::Invoke(txn3.into());
        let txn4 = Transaction::Invoke(txn4.into());
        let txn5 = Transaction::Invoke(txn5.into());
        let txn6 = Transaction::Invoke(txn6.into());
        let txn_reverted = Transaction::Invoke(txn_reverted.into());
        let mut receipt1 = receipt0.clone();
        let mut receipt2 = receipt0.clone();
        let mut receipt3 = receipt0.clone();
        let mut receipt4 = receipt0.clone();
        let mut receipt5 = receipt0.clone();
        let mut receipt_reverted = receipt0.clone();
        let mut receipt6 = Receipt {
            l2_to_l1_messages: vec![L2ToL1Message {
                from_address: contract_address!("0xcafebabe"),
                payload: vec![
                    l2_to_l1_message_payload_elem!("0x1"),
                    l2_to_l1_message_payload_elem!("0x2"),
                    l2_to_l1_message_payload_elem!("0x3"),
                ],
                to_address: EthereumAddress(H160::zero()),
            }],
            ..receipt0.clone()
        };
        receipt0.events = vec![Event {
            data: vec![event_data_bytes!(b"event 0 data")],
            from_address: contract_address_bytes!(b"event 0 from addr"),
            keys: vec![event_key_bytes!(b"event 0 key")],
        }];
        receipt1.transaction_hash = txn1_hash;
        receipt2.transaction_hash = txn2_hash;
        receipt3.transaction_hash = txn3_hash;
        receipt4.transaction_hash = txn4_hash;
        receipt5.transaction_hash = txn5_hash;
        receipt6.transaction_hash = txn6_hash;
        receipt_reverted.transaction_hash = txn_reverted_hash;
        receipt_reverted.execution_status = ExecutionStatus::Reverted;
        receipt_reverted.revert_error = Some("Reverted because".to_owned());

        let transaction_data0 = [(txn0, receipt0)];
        let transaction_data1 = [(txn1, receipt1), (txn2, receipt2)];
        let transaction_data2 = [
            (txn3, receipt3),
            (txn4, receipt4),
            (txn5, receipt5),
            (txn6, receipt6),
            (txn_reverted, receipt_reverted),
        ];
        db_txn
            .insert_transaction_data(header0.hash, header0.number, &transaction_data0)
            .unwrap();
        db_txn
            .insert_transaction_data(header1.hash, header1.number, &transaction_data1)
            .unwrap();
        db_txn
            .insert_transaction_data(header2.hash, header2.number, &transaction_data2)
            .unwrap();

        db_txn
            .insert_state_update(header0.number, &state_update0)
            .unwrap();
        db_txn
            .insert_state_update(header1.number, &state_update1)
            .unwrap();
        db_txn
            .insert_state_update(header2.number, &state_update2)
            .unwrap();

        // Mark block 0 as L1 accepted.
        db_txn.update_l1_l2_pointer(Some(header0.number)).unwrap();

        db_txn.commit().unwrap();
        storage
    }

    /// Creates [PendingData] which correctly links to the provided [Storage].
    ///
    /// i.e. the pending block's parent hash will be the latest block's hash from storage,
    /// and similarly for the pending state diffs state root.
    pub async fn create_pending_data(storage: Storage) -> PendingData {
        let storage2 = storage.clone();
        let latest = tokio::task::spawn_blocking(move || {
            let mut db = storage2.connection().unwrap();
            let tx = db.transaction().unwrap();

            tx.block_header(BlockId::Latest)
                .unwrap()
                .expect("Storage should contain a latest block")
        })
        .await
        .unwrap();

        let transactions: Vec<Transaction> = vec![
            InvokeTransaction::V0(InvokeTransactionV0 {
                calldata: vec![],
                sender_address: contract_address_bytes!(b"pending contract addr 0"),
                entry_point_selector: entry_point_bytes!(b"entry point 0"),
                entry_point_type: Some(EntryPointType::External),
                max_fee: crate::v02::types::request::Call::DEFAULT_MAX_FEE,
                signature: vec![],
                transaction_hash: transaction_hash_bytes!(b"pending tx hash 0"),
            })
            .into(),
            DeployTransaction {
                contract_address: contract_address!("0x1122355"),
                contract_address_salt: contract_address_salt_bytes!(b"salty"),
                class_hash: class_hash_bytes!(b"pending class hash 1"),
                constructor_calldata: vec![],
                transaction_hash: transaction_hash_bytes!(b"pending tx hash 1"),
                version: TransactionVersion(H256::zero()),
            }
            .into(),
            // Will be a reverted txn.
            InvokeTransaction::V0(InvokeTransactionV0 {
                calldata: vec![],
                sender_address: contract_address_bytes!(b"pending contract addr 0"),
                entry_point_selector: entry_point_bytes!(b"entry point 0"),
                entry_point_type: Some(EntryPointType::External),
                max_fee: crate::v02::types::request::Call::DEFAULT_MAX_FEE,
                signature: vec![],
                transaction_hash: transaction_hash_bytes!(b"pending reverted"),
            })
            .into(),
        ];

        let transaction_receipts = vec![
            Receipt {
                actual_fee: None,
                events: vec![
                    Event {
                        data: vec![],
                        from_address: contract_address!("0xabcddddddd"),
                        keys: vec![event_key_bytes!(b"pending key")],
                    },
                    Event {
                        data: vec![],
                        from_address: contract_address!("0xabcddddddd"),
                        keys: vec![event_key_bytes!(b"pending key")],
                    },
                    Event {
                        data: vec![],
                        from_address: contract_address!("0xabcaaaaaaa"),
                        keys: vec![event_key_bytes!(b"pending key 2")],
                    },
                ],
                execution_resources: Some(ExecutionResources {
                    builtin_instance_counter: Default::default(),
                    n_memory_holes: 0,
                    n_steps: 0,
                }),
                l1_to_l2_consumed_message: None,
                l2_to_l1_messages: vec![],
                transaction_hash: transactions[0].hash(),
                transaction_index: TransactionIndex::new_or_panic(0),
                execution_status: Default::default(),
                revert_error: Default::default(),
            },
            Receipt {
                actual_fee: None,
                events: vec![],
                execution_resources: Some(ExecutionResources {
                    builtin_instance_counter: Default::default(),
                    n_memory_holes: 0,
                    n_steps: 0,
                }),
                l1_to_l2_consumed_message: None,
                l2_to_l1_messages: vec![],
                transaction_hash: transactions[1].hash(),
                transaction_index: TransactionIndex::new_or_panic(1),
                execution_status: Default::default(),
                revert_error: Default::default(),
            },
            // Reverted and without events
            Receipt {
                actual_fee: None,
                events: vec![],
                execution_resources: Some(ExecutionResources {
                    builtin_instance_counter: Default::default(),
                    n_memory_holes: 0,
                    n_steps: 0,
                }),
                l1_to_l2_consumed_message: None,
                l2_to_l1_messages: vec![],
                transaction_hash: transactions[2].hash(),
                transaction_index: TransactionIndex::new_or_panic(2),
                execution_status:
                    starknet_gateway_types::reply::transaction::ExecutionStatus::Reverted,
                revert_error: Some("Reverted!".to_owned()),
            },
        ];

        let block = starknet_gateway_types::reply::PendingBlock {
            gas_price: GasPrice::from_be_slice(b"gas price").unwrap(),
            parent_hash: latest.hash,
            sequencer_address: sequencer_address_bytes!(b"pending sequencer address"),
            status: starknet_gateway_types::reply::Status::Pending,
            timestamp: BlockTimestamp::new_or_panic(1234567),
            transaction_receipts,
            transactions,
            starknet_version: StarknetVersion::new(0, 11, 0),
        };

        let contract1 = contract_address_bytes!(b"pending contract 1 address");
        let state_update = StateUpdate::default()
            .with_parent_state_commitment(latest.state_commitment)
            .with_declared_cairo_class(class_hash_bytes!(b"pending class 0 hash"))
            .with_declared_cairo_class(class_hash_bytes!(b"pending class 1 hash"))
            .with_deployed_contract(
                contract_address_bytes!(b"pending contract 0 address"),
                class_hash_bytes!(b"pending class 0 hash"),
            )
            .with_deployed_contract(contract1, class_hash_bytes!(b"pending class 1 hash"))
            .with_storage_update(
                contract1,
                storage_address_bytes!(b"pending storage key 0"),
                storage_value_bytes!(b"pending storage value 0"),
            )
            .with_storage_update(
                contract1,
                storage_address_bytes!(b"pending storage key 1"),
                storage_value_bytes!(b"pending storage value 1"),
            )
            // This is not a real contract and should be re-worked..
            .with_replaced_class(
                contract_address_bytes!(b"pending contract 2 (replaced)"),
                class_hash_bytes!(b"pending class 2 hash (replaced)"),
            );

        // The class definitions must be inserted into the database.
        let state_update_copy = state_update.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = storage.connection().unwrap();
            let tx = db.transaction().unwrap();
            let class_definition =
                starknet_gateway_test_fixtures::class_definitions::CONTRACT_DEFINITION;

            for cairo in state_update_copy.declared_cairo_classes {
                tx.insert_cairo_class(cairo, class_definition).unwrap();
            }

            for (sierra, casm) in state_update_copy.declared_sierra_classes {
                tx.insert_sierra_class(&sierra, b"sierra def", &casm, b"casm def", "test version")
                    .unwrap();
            }

            tx.commit().unwrap();
        })
        .await
        .unwrap();

        let pending_data = PendingData::default();
        pending_data
            .set(Arc::new(block), Arc::new(state_update))
            .await;
        pending_data
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn roundtrip_syncing() {
        use crate::v02::types::syncing::{NumberedBlock, Status, Syncing};

        let examples = [
            (line!(), "false", Syncing::False(false)),
            // this shouldn't exist but it exists now
            (line!(), "true", Syncing::False(true)),
            (
                line!(),
                r#"{"starting_block_hash":"0xa","starting_block_num":"0x1","current_block_hash":"0xb","current_block_num":"0x2","highest_block_hash":"0xc","highest_block_num":"0x3"}"#,
                Syncing::Status(Status {
                    starting: NumberedBlock::from(("a", 1)),
                    current: NumberedBlock::from(("b", 2)),
                    highest: NumberedBlock::from(("c", 3)),
                }),
            ),
        ];

        for (line, input, expected) in examples {
            let parsed = serde_json::from_str::<Syncing>(input).unwrap();
            let output = serde_json::to_string(&parsed).unwrap();

            assert_eq!(parsed, expected, "example from line {line}");
            assert_eq!(&output, input, "example from line {line}");
        }
    }

    #[tokio::test]
    async fn empty_get_on_root_is_ok() {
        // Monitoring bots often get query `/` with no body as a form
        // of health check. Test that we return success for such queries.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let context = RpcContext::for_tests();
        let (_jh, addr) = RpcServer::new(addr, context, DefaultVersion::V04)
            .spawn()
            .unwrap();

        let url = format!("http://{addr}/");

        let client = reqwest::Client::new();
        // No body
        let status = client.get(url.clone()).send().await.unwrap().status();
        assert!(status.is_success());
        // Empty body - unsure if this is actually different to no body.
        let status = client
            .get(url.clone())
            .body("")
            .send()
            .await
            .unwrap()
            .status();
        assert!(status.is_success());
        // Non-empty body should fail.
        let status = client
            .get(url.clone())
            .body("x")
            .send()
            .await
            .unwrap()
            .status();
        assert!(!status.is_success());
    }

    #[rustfmt::skip]
    #[rstest::rstest]
    // Ensure that a missing method would actually fail this test.
    #[should_panic]
    #[case::invalid("/", "invalid")]

    #[case::root_starknet_blockHashAndNumber("/", "starknet_blockHashAndNumber")]
    #[case::root_starknet_blockNumber("/", "starknet_blockNumber")]
    #[case::root_starknet_call("/", "starknet_call")]
    #[case::root_starknet_chainId("/", "starknet_chainId")]
    #[case::root_starknet_getBlockWithTxHashes("/", "starknet_getBlockWithTxHashes")]
    #[case::root_starknet_getBlockTransactionCount("/", "starknet_getBlockTransactionCount")]
    #[case::root_starknet_getClass("/", "starknet_getClass")]
    #[case::root_starknet_getClassAt("/", "starknet_getClassAt")]
    #[case::root_starknet_getClassHashAt("/", "starknet_getClassHashAt")]
    #[case::root_starknet_getNonce("/", "starknet_getNonce")]
    #[case::root_starknet_getStorageAt("/", "starknet_getStorageAt")]
    #[case::root_starknet_estimateFee("/", "starknet_estimateFee")]
    #[case::root_starknet_getEvents("/", "starknet_getEvents")]
    #[case::root_starknet_getStateUpdate("/", "starknet_getStateUpdate")]
    #[case::root_starknet_addDeclareTransaction("/", "starknet_addDeclareTransaction")]
    #[case::root_starknet_addDeployAccountTransaction("/", "starknet_addDeployAccountTransaction")]
    #[case::root_starknet_addInvokeTransaction("/", "starknet_addInvokeTransaction")]
    #[case::root_starknet_getBlockWithTxs("/", "starknet_getBlockWithTxs")]
    #[case::root_starknet_getTransactionReceipt("/", "starknet_getTransactionReceipt")]
    #[case::root_starknet_syncing("/", "starknet_syncing")]
    #[case::root_starknet_simulateTransactions("/", "starknet_simulateTransactions")]
    #[case::root_starknet_estimateMessageFee("/", "starknet_estimateMessageFee")]
    #[case::root_starknet_getTransactionByBlockIdAndIndex("/", "starknet_getTransactionByBlockIdAndIndex")]
    #[case::root_starknet_getTransactionByHash("/", "starknet_getTransactionByHash")]
    #[case::root_starknet_pendingTransactions("/", "starknet_pendingTransactions")]
    #[case::root_pathfinder_getProof("/", "pathfinder_getProof")]
    #[case::root_pathfinder_getTransactionStatus("/", "pathfinder_getTransactionStatus")]

    #[case::v03_starknet_addDeclareTransaction("/rpc/v0.3", "starknet_addDeclareTransaction")]
    #[case::v03_starknet_addDeployAccountTransaction("/rpc/v0.3", "starknet_addDeployAccountTransaction")]
    #[case::v03_starknet_addInvokeTransaction("/rpc/v0.3", "starknet_addInvokeTransaction")]
    #[case::v03_starknet_blockHashAndNumber("/rpc/v0.3", "starknet_blockHashAndNumber")]
    #[case::v03_starknet_blockNumber("/rpc/v0.3", "starknet_blockNumber")]
    #[case::v03_starknet_call("/rpc/v0.3", "starknet_call")]
    #[case::v03_starknet_chainId("/rpc/v0.3", "starknet_chainId")]
    #[case::v03_starknet_getBlockWithTxHashes("/rpc/v0.3", "starknet_getBlockWithTxHashes")]
    #[case::v03_starknet_getBlockWithTxs("/rpc/v0.3", "starknet_getBlockWithTxs")]
    #[case::v03_starknet_getBlockTransactionCount("/rpc/v0.3", "starknet_getBlockTransactionCount")]
    #[case::v03_starknet_getClass("/rpc/v0.3", "starknet_getClass")]
    #[case::v03_starknet_getClassAt("/rpc/v0.3", "starknet_getClassAt")]
    #[case::v03_starknet_getClassHashAt("/rpc/v0.3", "starknet_getClassHashAt")]
    #[case::v03_starknet_getNonce("/rpc/v0.3", "starknet_getNonce")]
    #[case::v03_starknet_getStorageAt("/rpc/v0.3", "starknet_getStorageAt")]
    #[case::v03_starknet_getTransactionByBlockIdAndIndex("/rpc/v0.3", "starknet_getTransactionByBlockIdAndIndex")]
    #[case::v03_starknet_getTransactionByHash("/rpc/v0.3", "starknet_getTransactionByHash")]
    #[case::v03_starknet_getTransactionReceipt("/rpc/v0.3", "starknet_getTransactionReceipt")]
    #[case::v03_starknet_pendingTransactions("/rpc/v0.3", "starknet_pendingTransactions")]
    #[case::v03_starknet_syncing("/rpc/v0.3", "starknet_syncing")]
    #[case::v03_starknet_estimateFee("/rpc/v0.3", "starknet_estimateFee")]
    #[case::v03_starknet_getEvents("/rpc/v0.3", "starknet_getEvents")]
    #[case::v03_starknet_getStateUpdate("/rpc/v0.3", "starknet_getStateUpdate")]
    #[case::v03_starknet_simulateTransaction("/rpc/v0.3", "starknet_simulateTransaction")]
    #[case::v03_starknet_estimateMessageFee("/rpc/v0.3", "starknet_estimateMessageFee")]
    #[case::v03_pathfinder_getProof("/rpc/v0.3", "pathfinder_getProof")]
    #[case::v03_pathfinder_getTransactionStatus("/rpc/v0.3", "pathfinder_getTransactionStatus")]

    #[case::v04_starknet_blockHashAndNumber("/rpc/v0.4", "starknet_blockHashAndNumber")]
    #[case::v04_starknet_blockNumber("/rpc/v0.4", "starknet_blockNumber")]
    #[case::v04_starknet_call("/rpc/v0.4", "starknet_call")]
    #[case::v04_starknet_chainId("/rpc/v0.4", "starknet_chainId")]
    #[case::v04_starknet_getBlockWithTxHashes("/rpc/v0.4", "starknet_getBlockWithTxHashes")]
    #[case::v04_starknet_getBlockTransactionCount("/rpc/v0.4", "starknet_getBlockTransactionCount")]
    #[case::v04_starknet_getClass("/rpc/v0.4", "starknet_getClass")]
    #[case::v04_starknet_getClassAt("/rpc/v0.4", "starknet_getClassAt")]
    #[case::v04_starknet_getClassHashAt("/rpc/v0.4", "starknet_getClassHashAt")]
    #[case::v04_starknet_getNonce("/rpc/v0.4", "starknet_getNonce")]
    #[case::v04_starknet_getStorageAt("/rpc/v0.4", "starknet_getStorageAt")]
    #[case::v04_starknet_estimateFee("/rpc/v0.4", "starknet_estimateFee")]
    #[case::v04_starknet_getEvents("/rpc/v0.4", "starknet_getEvents")]
    #[case::v04_starknet_getStateUpdate("/rpc/v0.4", "starknet_getStateUpdate")]
    #[case::v04_starknet_addDeclareTransaction("/rpc/v0.4", "starknet_addDeclareTransaction")]
    #[case::v04_starknet_addDeployAccountTransaction("/rpc/v0.4", "starknet_addDeployAccountTransaction")]
    #[case::v04_starknet_addInvokeTransaction("/rpc/v0.4", "starknet_addInvokeTransaction")]
    #[case::v04_starknet_getBlockWithTxs("/rpc/v0.4", "starknet_getBlockWithTxs")]
    #[case::v04_starknet_getTransactionReceipt("/rpc/v0.4", "starknet_getTransactionReceipt")]
    #[case::v04_starknet_syncing("/rpc/v0.4", "starknet_syncing")]
    #[case::v04_starknet_simulateTransactions("/rpc/v0.4", "starknet_simulateTransactions")]
    #[case::v04_starknet_estimateMessageFee("/rpc/v0.4", "starknet_estimateMessageFee")]
    #[case::v04_starknet_getTransactionByBlockIdAndIndex("/rpc/v0.4", "starknet_getTransactionByBlockIdAndIndex")]
    #[case::v04_starknet_getTransactionByHash("/rpc/v0.4", "starknet_getTransactionByHash")]
    #[case::v04_starknet_pendingTransactions("/rpc/v0.4", "starknet_pendingTransactions")]
    #[case::v04_pathfinder_getProof("/rpc/v0.4", "pathfinder_getProof")]
    #[case::v04_pathfinder_getTransactionStatus("/rpc/v0.4", "pathfinder_getTransactionStatus")]

    #[case::pathfinder_pathfinder_version("/rpc/pathfinder/v0.1", "pathfinder_version")]
    #[case::pathfinder_pathfinder_getProof("/rpc/pathfinder/v0.1", "pathfinder_getProof")]
    #[case::pathfinder_pathfinder_getTransactionStatus("/rpc/pathfinder/v0.1", "pathfinder_getTransactionStatus")]

    #[allow(non_snake_case)]
    #[tokio::test]
    async fn rpc_routing(#[case] route: &'static str, #[case] method: &'static str) {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let context = RpcContext::for_tests();
        let (_jh, addr) = RpcServer::new(addr, context, DefaultVersion::V04)
            .spawn()
            .unwrap();

        let url = format!("http://{addr}{route}");
        let client = reqwest::Client::new();

        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "id": 0,
        });

        let res: serde_json::Value = client
            .post(url.clone())
            .json(&request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let method_not_found = json!(-32601);

        assert_ne!(dbg!(&res["error"]["code"]), dbg!(&method_not_found));
    }
}
