//
// rollup_executor.rs
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

use crate::ar_toolbox::ArToolBox;
use arc_swap::ArcSwapOption;
use db3_base::times;
use db3_error::{DB3Error, Result};
use db3_proto::db3_rollup_proto::{GcRecord, RollupRecord};
use db3_storage::ar_fs::{ArFileSystem, ArFileSystemConfig};
use db3_storage::meta_store_client::MetaStoreClient;
use db3_storage::mutation_store::MutationStore;
use db3_storage::system_store::{SystemRole, SystemStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Clone)]
pub struct RollupExecutorConfig {
    pub temp_data_path: String,
    pub key_root_path: String,
}

pub struct RollupExecutor {
    config: RollupExecutorConfig,
    storage: MutationStore,
    ar_toolbox: ArcSwapOption<ArToolBox>,
    min_rollup_size: Arc<AtomicU64>,
    meta_store: ArcSwapOption<MetaStoreClient>,
    pending_mutations: Arc<AtomicU64>,
    pending_data_size: Arc<AtomicU64>,
    pending_start_block: Arc<AtomicU64>,
    pending_end_block: Arc<AtomicU64>,
    network_id: Arc<AtomicU64>,
    system_store: Arc<SystemStore>,
    rollup_max_interval: Arc<AtomicU64>,
    min_gc_round_offset: Arc<AtomicU64>,
}

unsafe impl Sync for RollupExecutor {}
unsafe impl Send for RollupExecutor {}

impl RollupExecutor {
    pub async fn new(
        config: RollupExecutorConfig,
        storage: MutationStore,
        system_store: Arc<SystemStore>,
    ) -> Result<Self> {
        if let Some(c) = system_store.get_config(&SystemRole::DataRollupNode)? {
            info!(
                "use persistence config to build rollup executor with config {:?}",
                c
            );
            let wallet = system_store.get_evm_wallet(c.chain_id)?;
            let min_rollup_size = c.min_rollup_size;
            let meta_store = ArcSwapOption::from(Some(Arc::new(
                MetaStoreClient::new(c.contract_addr.as_str(), c.evm_node_url.as_str(), wallet)
                    .await?,
            )));
            let ar_fs_config = ArFileSystemConfig {
                arweave_url: c.ar_node_url.clone(),
                key_root_path: config.key_root_path.clone(),
            };
            let ar_filesystem = ArFileSystem::new(ar_fs_config)?;
            let ar_toolbox = ArcSwapOption::from(Some(Arc::new(ArToolBox::new(
                ar_filesystem,
                config.temp_data_path.clone(),
            )?)));
            let rollup_max_interval = Arc::new(AtomicU64::new(c.rollup_max_interval));
            Ok(Self {
                config,
                storage,
                ar_toolbox,
                min_rollup_size: Arc::new(AtomicU64::new(min_rollup_size)),
                meta_store,
                pending_mutations: Arc::new(AtomicU64::new(0)),
                pending_data_size: Arc::new(AtomicU64::new(0)),
                pending_start_block: Arc::new(AtomicU64::new(0)),
                pending_end_block: Arc::new(AtomicU64::new(0)),
                network_id: Arc::new(AtomicU64::new(c.network_id)),
                system_store,
                rollup_max_interval,
                min_gc_round_offset: Arc::new(AtomicU64::new(c.min_gc_offset)),
            })
        } else {
            let rollup_max_interval = Arc::new(AtomicU64::new(0));
            Ok(Self {
                config,
                storage,
                ar_toolbox: ArcSwapOption::from(None),
                min_rollup_size: Arc::new(AtomicU64::new(0)),
                meta_store: ArcSwapOption::from(None),
                pending_mutations: Arc::new(AtomicU64::new(0)),
                pending_data_size: Arc::new(AtomicU64::new(0)),
                pending_start_block: Arc::new(AtomicU64::new(0)),
                pending_end_block: Arc::new(AtomicU64::new(0)),
                network_id: Arc::new(AtomicU64::new(0)),
                system_store,
                rollup_max_interval,
                min_gc_round_offset: Arc::new(AtomicU64::new(0)),
            })
        }
    }

    ///
    /// call by the update hook
    ///
    pub async fn update_config(&self) -> Result<()> {
        if let Some(c) = self.system_store.get_config(&SystemRole::DataRollupNode)? {
            info!(
                "update the new system config {:?} for the rollup executor",
                c
            );
            let wallet = self.system_store.get_evm_wallet(c.chain_id)?;
            self.min_rollup_size
                .store(c.min_rollup_size, Ordering::Relaxed);
            self.rollup_max_interval
                .store(c.rollup_max_interval, Ordering::Relaxed);
            let meta_store = Some(Arc::new(
                MetaStoreClient::new(c.contract_addr.as_str(), c.evm_node_url.as_str(), wallet)
                    .await?,
            ));
            self.min_gc_round_offset
                .store(c.min_gc_offset, Ordering::Relaxed);
            self.meta_store.store(meta_store);
            let ar_fs_config = ArFileSystemConfig {
                arweave_url: c.ar_node_url.clone(),
                key_root_path: self.config.key_root_path.clone(),
            };
            let ar_filesystem = ArFileSystem::new(ar_fs_config)?;
            let ar_toolbox = Some(Arc::new(ArToolBox::new(
                ar_filesystem,
                self.config.temp_data_path.clone(),
            )?));
            self.ar_toolbox.store(ar_toolbox);
            self.network_id.store(c.network_id, Ordering::Relaxed);
        }
        Ok(())
    }

    fn gc_mutation(&self) -> Result<()> {
        let (last_start_block, last_end_block, first) = match self.storage.get_last_gc_record()? {
            Some(r) => (r.start_block, r.end_block, false),
            None => (0_u64, 0_u64, true),
        };

        info!(
            "last gc block range [{}, {})",
            last_start_block, last_end_block
        );

        let now = Instant::now();
        if self.storage.has_enough_round_left(
            last_start_block,
            self.min_gc_round_offset.load(Ordering::Relaxed),
        )? {
            if first {
                if let Some(r) = self.storage.get_rollup_record(last_start_block)? {
                    self.storage.gc_range_mutation(r.start_block, r.end_block)?;
                    let record = GcRecord {
                        start_block: r.start_block,
                        end_block: r.end_block,
                        data_size: r.raw_data_size,
                        time: times::get_current_time_in_secs(),
                        processed_time: now.elapsed().as_secs(),
                    };
                    self.storage.add_gc_record(&record)?;
                    info!(
                        "gc mutation from block range [{}, {}) done",
                        r.start_block, r.end_block
                    );
                    Ok(())
                } else {
                    // going here is not normal case
                    warn!(
                        "fail to get next rollup record with start block {}",
                        last_start_block
                    );
                    Ok(())
                }
            } else {
                if let Some(r) = self.storage.get_next_rollup_record(last_start_block)? {
                    self.storage.gc_range_mutation(r.start_block, r.end_block)?;
                    let record = GcRecord {
                        start_block: r.start_block,
                        end_block: r.end_block,
                        data_size: r.raw_data_size,
                        time: times::get_current_time_in_secs(),
                        processed_time: now.elapsed().as_secs(),
                    };
                    self.storage.add_gc_record(&record)?;
                    info!(
                        "gc mutation from block range [{}, {}) done",
                        r.start_block, r.end_block
                    );
                    Ok(())
                } else {
                    // going here is not normal case
                    warn!(
                        "fail to get next rollup record with start block {}",
                        last_start_block
                    );
                    Ok(())
                }
            }
        } else {
            info!("not enough round to run gc");
            Ok(())
        }
    }

    pub fn get_pending_rollup(&self) -> RollupRecord {
        RollupRecord {
            end_block: self.pending_end_block.load(Ordering::Relaxed),
            start_block: self.pending_start_block.load(Ordering::Relaxed),
            raw_data_size: self.pending_data_size.load(Ordering::Relaxed),
            compress_data_size: 0,
            processed_time: 0,
            arweave_tx: "".to_string(),
            time: times::get_current_time_in_secs(),
            mutation_count: self.pending_mutations.load(Ordering::Relaxed),
            cost: 0,
            evm_tx: "".to_string(),
            evm_cost: 0,
        }
    }

    pub async fn process(&self) -> Result<()> {
        if let (Some(ref meta_store), Some(ref ar_toolbox)) =
            (self.meta_store.load_full(), self.ar_toolbox.load_full())
        {
            let network_id = self.network_id.load(Ordering::Relaxed);
            self.storage.flush_state()?;
            let (last_start_block, last_end_block, tx) =
                match self.storage.get_last_rollup_record()? {
                    Some(r) => (r.start_block, r.end_block, r.arweave_tx.to_string()),
                    _ => (0_u64, 0_u64, "".to_string()),
                };
            let current_block = self.storage.get_current_block()?;
            if current_block <= last_end_block {
                info!("no block to rollup");
                return Ok(());
            }
            let now = Instant::now();
            info!(
                "the next rollup start block {} and the newest block {current_block}",
                last_end_block
            );
            self.pending_start_block
                .store(last_start_block, Ordering::Relaxed);
            self.pending_end_block
                .store(current_block, Ordering::Relaxed);
            let mutations = self
                .storage
                .get_range_mutations(last_end_block, current_block)?;
            if mutations.len() <= 0 {
                info!("no block to rollup");
                return Ok(());
            }
            self.pending_mutations
                .store(mutations.len() as u64, Ordering::Relaxed);
            let recordbatch = ar_toolbox.convert_mutations_to_recordbatch(&mutations)?;
            let memory_size = recordbatch.get_array_memory_size();
            self.pending_data_size
                .store(memory_size as u64, Ordering::Relaxed);
            if memory_size < self.min_rollup_size.load(Ordering::Relaxed) as usize {
                info!(
                "there not enough data to trigger rollup, the min_rollup_size {}, current size {}",
                self.min_rollup_size.load(Ordering::Relaxed), memory_size
            );
                return Ok(());
            } else {
                self.pending_start_block
                    .store(current_block, Ordering::Relaxed);
                self.pending_end_block
                    .store(current_block, Ordering::Relaxed);
                self.pending_data_size.store(0, Ordering::Relaxed);
                self.pending_mutations.store(0, Ordering::Relaxed);
            }
            let (id, reward, num_rows, size) = ar_toolbox
                .compress_and_upload_record_batch(
                    tx,
                    last_end_block,
                    current_block,
                    &recordbatch,
                    network_id,
                )
                .await?;
            let (evm_cost, tx_hash) = meta_store
                .update_rollup_step(id.as_str(), network_id)
                .await?;
            let tx_str = format!("0x{}", hex::encode(tx_hash.as_bytes()));
            info!("the process rollup done with num mutations {num_rows}, raw data size {memory_size}, compress data size {size} and processed time {} id {} ar cost {} and evm tx {} and cost {}", now.elapsed().as_secs(),
                id.as_str(), reward,
                tx_str.as_str(),
                evm_cost.as_u64()
                );
            let record = RollupRecord {
                end_block: current_block,
                raw_data_size: memory_size as u64,
                compress_data_size: size,
                processed_time: now.elapsed().as_secs(),
                arweave_tx: id,
                time: times::get_current_time_in_secs(),
                mutation_count: num_rows,
                cost: reward,
                start_block: last_end_block,
                evm_tx: tx_str,
                evm_cost: evm_cost.as_u64(),
            };
            self.storage
                .add_rollup_record(&record)
                .map_err(|e| DB3Error::RollupError(format!("{e}")))?;
            self.gc_mutation()?;
        } else {
            warn!("the system has not been setup, please setup it first");
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use db3_crypto::db3_address::DB3Address;
    use db3_proto::db3_base_proto::SystemConfig;
    use db3_proto::db3_mutation_v2_proto::MutationAction;
    use db3_storage::db_store_v2::{DBStoreV2, DBStoreV2Config};
    use db3_storage::doc_store::DocStoreConfig;
    use db3_storage::mutation_store::MutationStoreConfig;
    use db3_storage::state_store::StateStore;
    use db3_storage::state_store::StateStoreConfig;
    use db3_storage::system_store::SystemStoreConfig;
    use std::path::PathBuf;
    use tempdir::TempDir;
    fn generate_config(
        real_path: &str,
    ) -> (
        StateStoreConfig,
        SystemStoreConfig,
        MutationStoreConfig,
        RollupExecutorConfig,
        DBStoreV2Config,
    ) {
        if let Err(_e) = std::fs::create_dir_all(real_path) {}
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let key_root_path = path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tools/keys")
            .to_str()
            .unwrap()
            .to_string();
        let rollup_config = RollupExecutorConfig {
            temp_data_path: format!("{real_path}/data_path"),
            key_root_path: key_root_path.to_string(),
        };
        let system_store_config = SystemStoreConfig {
            key_root_path: key_root_path.to_string(),
            evm_wallet_key: "evm".to_string(),
            ar_wallet_key: "ar".to_string(),
        };

        let store_config = MutationStoreConfig {
            db_path: format!("{real_path}/mutation_path"),
            block_store_cf_name: "block_store_cf".to_string(),
            tx_store_cf_name: "tx_store_cf".to_string(),
            rollup_store_cf_name: "rollup_store_cf".to_string(),
            gc_cf_name: "gc_store_cf".to_string(),
            message_max_buffer: 4 * 1024,
            scan_max_limit: 50,
            block_state_cf_name: "block_state_cf".to_string(),
        };
        let state_config = StateStoreConfig {
            db_path: format!("{real_path}/state_store"),
        };

        let db_store_config = DBStoreV2Config {
            db_path: format!("{real_path}/db_store"),
            db_store_cf_name: "db".to_string(),
            doc_store_cf_name: "doc".to_string(),
            collection_store_cf_name: "cf2".to_string(),
            index_store_cf_name: "index".to_string(),
            doc_owner_store_cf_name: "doc_owner".to_string(),
            db_owner_store_cf_name: "db_owner".to_string(),
            scan_max_limit: 50,
            enable_doc_store: false,
            doc_store_conf: DocStoreConfig::default(),
            doc_start_id: 1000,
        };

        (
            state_config,
            system_store_config,
            store_config,
            rollup_config,
            db_store_config,
        )
    }

    async fn setup_for_smoke_test() -> Result<RollupExecutor> {
        let tmp_dir_path = TempDir::new("add_store_path").expect("create temp dir");
        let real_path = tmp_dir_path.path().to_str().unwrap().to_string();
        let (state_config, system_store_config, store_config, rollup_config, _) =
            generate_config(real_path.as_str());
        let state_store = Arc::new(StateStore::new(state_config).unwrap());
        let system_store = Arc::new(SystemStore::new(system_store_config, state_store.clone()));
        let storage = MutationStore::new(store_config).unwrap();
        storage.recover().unwrap();
        let system_config = SystemConfig {
            min_rollup_size: 1024,
            rollup_interval: 1000,
            network_id: 1,
            evm_node_url: "ws://127.0.0.1:8545".to_string(),
            ar_node_url: "http://127.0.0.1:1985".to_string(),
            chain_id: 31337_u32,
            rollup_max_interval: 2000,
            contract_addr: "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
            min_gc_offset: 100,
        };
        let result = system_store.update_config(&SystemRole::DataRollupNode, &system_config);
        assert_eq!(true, result.is_ok());
        for _i in 0..1000 {
            let payload: Vec<u8> = vec![1];
            let signature: &str = "0xasdasdsad";
            let (_id, block, order) = storage
                .generate_mutation_block_and_order(payload.as_ref(), signature)
                .unwrap();
            let result = storage.add_mutation(
                payload.as_ref(),
                signature,
                "",
                &DB3Address::ZERO,
                1,
                block,
                order,
                1,
                MutationAction::CreateDocumentDb,
            );
            assert_eq!(true, result.is_ok());
        }
        RollupExecutor::new(rollup_config, storage, system_store).await
    }

    #[tokio::test]
    async fn test_rollup_smoke_test() {
        match setup_for_smoke_test().await {
            Ok(rollup_executor) => {
                let result = rollup_executor.process().await;
                assert_eq!(true, result.is_ok());
            }
            Err(e) => {
                assert!(false, "{e}");
            }
        }
    }
}
