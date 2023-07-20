//
// indexer_impl.rs
// Copyright (C) 2023 db3.network Author imotai <codego.me@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use crate::mutation_utils::MutationUtil;
use crate::recover::{Recover, RecoverConfig};
use crate::version_util;
use db3_crypto::db3_address::DB3Address;
use db3_error::{DB3Error, Result};
use db3_event::event_processor::EventProcessor;
use db3_event::event_processor::EventProcessorConfig;
use db3_proto::db3_base_proto::SystemStatus;
use db3_proto::db3_database_v2_proto::BlockState;
use db3_proto::db3_indexer_proto::indexer_node_server::IndexerNode;
use db3_proto::db3_indexer_proto::{
    ContractSyncStatus, GetContractSyncStatusRequest, GetContractSyncStatusResponse,
    GetSystemStatusRequest, RunQueryRequest, RunQueryResponse, SetupRequest, SetupResponse,
};
use db3_proto::db3_mutation_v2_proto::MutationAction;
use db3_proto::db3_storage_proto::block_response::MutationWrapper;
use db3_proto::db3_storage_proto::event_message;
use db3_proto::db3_storage_proto::EventMessage as EventMessageV2;
use db3_sdk::store_sdk_v2::StoreSDKV2;
use db3_storage::db_store_v2::{DBStoreV2, DBStoreV2Config};
use db3_storage::key_store::{KeyStore, KeyStoreConfig};
use db3_storage::state_store::{StateStore, StateStoreConfig};
use ethers::abi::Address;
use ethers::prelude::{LocalWallet, Signer};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task;
use tokio::time::{sleep, Duration};
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

const MAX_BLOCK_ID: u64 = u64::MAX;
#[derive(Clone)]
pub struct IndexerNodeImpl {
    db_store: DBStoreV2,
    recover_config: RecoverConfig,
    network_id: Arc<AtomicU64>,
    chain_id: Arc<AtomicU32>,
    node_url: String,
    key_root_path: String,
    contract_addr: String,
    evm_node_url: String,
    processor_mapping: Arc<Mutex<HashMap<String, Arc<EventProcessor>>>>,
    admin_addr: String,
}

impl IndexerNodeImpl {
    pub async fn new(
        config: DBStoreV2Config,
        network_id: u64,
        chain_id: u32,
        node_url: String,
        ar_node_url: String,
        key_root_path: String,
        contract_addr: String,
        evm_node_url: String,
        admin_addr: String,
        recover_data_path: String,
    ) -> Result<Self> {
        let db_store = DBStoreV2::new(config.clone())?;
        let network_id = Arc::new(AtomicU64::new(network_id));
        let chain_id = Arc::new(AtomicU32::new(chain_id));
        let recover_config = RecoverConfig {
            key_root_path: key_root_path.clone(),
            ar_node_url: ar_node_url.clone(),
            temp_data_path: recover_data_path,
            contract_addr: contract_addr.to_string(),
            evm_node_url: evm_node_url.to_string(),
            enable_mutation_recover: true,
        };
        Ok(Self {
            db_store,
            recover_config,
            network_id: network_id.clone(),
            chain_id: chain_id.clone(),
            node_url,
            key_root_path,
            contract_addr,
            evm_node_url,
            //TODO recover from the database
            processor_mapping: Arc::new(Mutex::new(HashMap::new())),
            admin_addr,
        })
    }

    pub async fn recover_state(&self) -> Result<()> {
        self.db_store.recover_db_state()?;
        let databases = self.db_store.get_all_event_db()?;
        for database in databases {
            let address_ref: &[u8] = database.address.as_ref();
            let db_address = DB3Address::try_from(address_ref)?;
            let (collections, _) = self.db_store.get_collection_of_database(&db_address)?;
            let tables = collections.iter().map(|c| c.name.to_string()).collect();
            if let Err(_e) = self
                .start_an_event_task(
                    &db_address,
                    database.evm_node_url.as_str(),
                    database.events_json_abi.as_str(),
                    &tables,
                    database.contract_address.as_str(),
                    0,
                )
                .await
            {
                info!("recover the event db {} has error", db_address.to_hex());
            } else {
                info!("recover the event db {} done", db_address.to_hex());
            }
        }
        Ok(())
    }

    /// start standalone indexer block syncer
    /// 1. subscribe db3 event
    /// 2. handle event to sync db3 node block
    pub async fn start(&self, store_sdk: StoreSDKV2) -> Result<()> {
        self.recover_state().await?;
        self.recover_from_ar().await?;
        self.recover_from_fetched_blocks(&store_sdk).await?;
        info!("start subscribe...");
        loop {
            match store_sdk.subscribe_event_message().await {
                Ok(handle) => {
                    info!("listen and handle event message");
                    let mut stream = handle.into_inner();
                    while let Some(event) = stream.message().await.unwrap() {
                        match self.handle_event(event, &store_sdk).await {
                            Err(e) => {
                                warn!("[IndexerBlockSyncer] handle event error: {:?}", e);
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    warn!("fail to subscribe block event for {e} and retry in 5 seconds");
                    sleep(Duration::from_millis(1000 * 5)).await;
                }
            }
        }
    }

    pub async fn recover_from_ar(&self) -> Result<()> {
        let recover = Recover::new(
            self.recover_config.clone(),
            self.network_id.clone(),
            self.chain_id.clone(),
            self.db_store.clone(),
        )
        .await?;

        info!("start recover from arweave");
        let last_block = self.db_store.recover_block_state()?;
        let (block, order) = match last_block {
            Some(block_state) => {
                info!(
                    "recover the block state done, last block is {:?}",
                    block_state
                );
                (block_state.block, block_state.order)
            }
            None => {
                info!("recover the block state done, last block is 0");
                (0, 0)
            }
        };
        recover.recover_from_arweave(block).await;
        info!("recover from arweave done!");
        Ok(())
    }
    /// recover from fetched blocks
    pub async fn recover_from_fetched_blocks(&self, store_sdk: &StoreSDKV2) -> Result<()> {
        info!("start recover from fetched blocks");
        let (mut start_block, mut order) = match self.db_store.recover_block_state()? {
            Some(block_state) => (block_state.block, block_state.order),
            None => (0, 0),
        };
        info!("start block is {}, order is {}", start_block, order);
        loop {
            let response = store_sdk
                .get_blocks(start_block, start_block + 1000)
                .await
                .map_err(|e| DB3Error::WriteStoreError(format!("{e}")))?
                .into_inner();
            info!(
                "fetch blocks from {} to {}",
                start_block,
                start_block + 1000
            );
            let mutations = response.mutations;
            info!(
                "Cold start with block mutations size: {:?}",
                mutations.len()
            );
            if mutations.is_empty() {
                info!("Stop fetching blocks, no more blocks to fetch");
                break;
            }
            self.parse_and_apply_mutations(&mutations).await?;
            start_block += 100;
        }
        info!("recover from fetched blocks done!");
        Ok(())
    }

    /// handle event message
    async fn handle_event(&self, event: EventMessageV2, store_sdk: &StoreSDKV2) -> Result<()> {
        match event.event {
            Some(event_message::Event::BlockEvent(be)) => {
                debug!(
                    "Receive BlockEvent: Block\t{}\tMutationCount\t{}",
                    be.block_id, be.mutation_count,
                );
                let block_state = match self.db_store.recover_block_state()? {
                    Some(block_state) => block_state,
                    None => BlockState { block: 0, order: 0 },
                };

                let response = store_sdk
                    .get_blocks(block_state.block, be.block_id)
                    .await
                    .map_err(|e| DB3Error::WriteStoreError(format!("{e}")))?
                    .into_inner();
                let mutations = response.mutations;
                debug!("Block mutations size: {:?}", mutations.len());
                self.parse_and_apply_mutations(&mutations).await?;
            }
            _ => {}
        }
        Ok(())
    }
    fn build_wallet(key_root_path: &str) -> Result<LocalWallet> {
        let config = KeyStoreConfig {
            key_root_path: key_root_path.to_string(),
        };
        let key_store = KeyStore::new(config);
        match key_store.has_key("evm") {
            true => {
                let data = key_store.get_key("evm")?;
                let data_ref: &[u8] = &data;
                let wallet = LocalWallet::from_bytes(data_ref)
                    .map_err(|e| DB3Error::RollupError(format!("{e}")))?;
                Ok(wallet)
            }

            false => {
                let mut rng = rand::thread_rng();
                let wallet = LocalWallet::new(&mut rng);
                let data = wallet.signer().to_bytes();
                key_store.write_key("evm", data.deref())?;
                Ok(wallet)
            }
        }
    }

    async fn start_an_event_task(
        &self,
        db: &DB3Address,
        evm_node_url: &str,
        abi: &str,
        tables: &Vec<String>,
        contract_address: &str,
        start_block: u64,
    ) -> Result<()> {
        let config = EventProcessorConfig {
            evm_node_url: evm_node_url.to_string(),
            db_addr: db.to_hex(),
            abi: abi.to_string(),
            target_events: tables.iter().map(|t| t.to_string()).collect(),
            contract_addr: contract_address.to_string(),
            start_block,
        };
        let processor = Arc::new(
            EventProcessor::new(config, self.db_store.clone())
                .await
                .map_err(|e| DB3Error::WriteStoreError(format!("{e}")))?,
        );
        match self.processor_mapping.lock() {
            Ok(mut mapping) => {
                //TODO limit the total count
                if mapping.contains_key(contract_address) {
                    warn!("contract addr {} exist", contract_address);
                    return Err(DB3Error::WriteStoreError(format!(
                        "contract_addr {} exist",
                        contract_address
                    )));
                }
                mapping.insert(contract_address.to_string(), processor.clone());
            }
            _ => todo!(),
        }

        task::spawn(async move {
            if let Err(e) = processor
                .start()
                .await
                .map_err(|e| DB3Error::WriteStoreError(format!("{e}")))
            {
                warn!("fail to start event processor for {e}");
            }
        });
        Ok(())
    }

    async fn parse_and_apply_mutations(&self, mutations: &Vec<MutationWrapper>) -> Result<()> {
        for mutation in mutations.iter() {
            let header = mutation.header.as_ref().unwrap();
            let body = mutation.body.as_ref().unwrap();
            // validate the signature
            let (dm, address, nonce) =
                MutationUtil::unwrap_and_light_verify(&body.payload, body.signature.as_str())
                    .map_err(|e| DB3Error::WriteStoreError(format!("{e}")))?;
            let action = MutationAction::from_i32(dm.action).ok_or(DB3Error::WriteStoreError(
                "fail to convert action type".to_string(),
            ))?;
            let (block, order, doc_ids_map_str) = match &mutation.header {
                Some(header) => Ok((header.block_id, header.order_id, &header.doc_ids_map)),
                _ => Err(DB3Error::WriteStoreError(
                    "invalid mutation header".to_string(),
                )),
            }?;
            let doc_ids_map = MutationUtil::convert_doc_ids_map_to_vec(doc_ids_map_str)?;
            self.db_store.apply_mutation(
                action,
                dm,
                &address,
                header.network,
                nonce,
                block,
                order,
                &doc_ids_map,
            )?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl IndexerNode for IndexerNodeImpl {
    async fn get_contract_sync_status(
        &self,
        _request: Request<GetContractSyncStatusRequest>,
    ) -> std::result::Result<Response<GetContractSyncStatusResponse>, Status> {
        let status_list: Vec<ContractSyncStatus> = match self.processor_mapping.lock() {
            Ok(mapping) => mapping
                .iter()
                .map(|ref processor| ContractSyncStatus {
                    addr: processor.1.get_config().contract_addr.to_string(),
                    evm_node_url: processor.1.get_config().evm_node_url.to_string(),
                    block_number: processor.1.get_block_number(),
                    event_number: processor.1.get_event_number(),
                })
                .collect(),
            _ => todo!(),
        };
        Ok(Response::new(GetContractSyncStatusResponse { status_list }))
    }

    async fn setup(
        &self,
        request: Request<SetupRequest>,
    ) -> std::result::Result<Response<SetupResponse>, Status> {
        let r = request.into_inner();
        let (addr, data) =
            MutationUtil::verify_setup(&r.payload, r.signature.as_str()).map_err(|e| {
                Status::invalid_argument(format!("fail to parse the payload and signature {e}"))
            })?;
        let admin_addr = self
            .admin_addr
            .parse::<Address>()
            .map_err(|e| Status::internal(format!("{e}")))?;
        if admin_addr != addr {
            return Err(Status::permission_denied(
                "You are not the admin".to_string(),
            ));
        }
        let network = MutationUtil::get_u64_field(&data, "network", 0_u64);
        self.network_id.store(network, Ordering::Relaxed);
        return Ok(Response::new(SetupResponse {
            code: 0,
            msg: "ok".to_string(),
        }));
    }

    async fn get_system_status(
        &self,
        _request: Request<GetSystemStatusRequest>,
    ) -> std::result::Result<Response<SystemStatus>, Status> {
        let wallet = Self::build_wallet(self.key_root_path.as_str())
            .map_err(|e| Status::internal(format!("{e}")))?;
        let addr = format!("0x{}", hex::encode(wallet.address().as_bytes()));
        Ok(Response::new(SystemStatus {
            evm_account: addr,
            evm_balance: "0".to_string(),
            ar_account: "".to_string(),
            ar_balance: "".to_string(),
            node_url: self.node_url.to_string(),
            config: None,
            has_inited: false,
            admin_addr: self.admin_addr.to_string(),
            version: Some(version_util::build_version()),
        }))
    }

    async fn run_query(
        &self,
        request: Request<RunQueryRequest>,
    ) -> std::result::Result<Response<RunQueryResponse>, Status> {
        let r = request.into_inner();
        let addr = DB3Address::from_hex(r.db.as_str()).map_err(|e| {
            Status::invalid_argument(format!("fail to parse the db address for {e}"))
        })?;
        if let Some(q) = &r.query {
            info!("query str {} q {:?}", q.query_str, q);
            let (documents, count) = self
                .db_store
                .query_docs(&addr, r.col_name.as_str(), q)
                .map_err(|e| Status::internal(format!("{e}")))?;
            info!(
                "query str {} from collection {} in db {} with result len {}, parameters len {}",
                q.query_str,
                r.col_name.as_str(),
                r.db.as_str(),
                documents.len(),
                q.parameters.len()
            );
            Ok(Response::new(RunQueryResponse { documents, count }))
        } else {
            Err(Status::invalid_argument("no query provided".to_string()))
        }
    }
}
#[cfg(test)]
mod tests {}
