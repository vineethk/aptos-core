// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    access_path_cache::AccessPathCache, data_cache::get_resource_group_from_metadata,
    move_vm_ext::AptosMoveResolver, transaction_metadata::TransactionMetadata,
};
use aptos_aggregator::{aggregator_extension::AggregatorID, delta_change_set::serialize};
use aptos_crypto::{hash::CryptoHash, HashValue};
use aptos_crypto_derive::{BCSCryptoHash, CryptoHasher};
use aptos_framework::natives::{
    aggregator_natives::{AggregatorChange, AggregatorChangeSet, NativeAggregatorContext},
    code::{NativeCodeContext, PublishRequest},
    event::NativeEventContext,
};
use aptos_table_natives::{NativeTableContext, TableChangeSet};
use aptos_types::{
    block_metadata::BlockMetadata,
    contract_event::ContractEvent,
    on_chain_config::{CurrentTimeMicroseconds, Features, OnChainConfig},
    state_store::{state_key::StateKey, state_value::StateValueMetadata},
    transaction::SignatureCheckedTransaction,
    write_set::WriteOp,
};
use aptos_vm_types::{change_set::VMChangeSet, storage::ChangeSetConfigs};
use move_binary_format::errors::{Location, PartialVMError, VMResult};
use move_core_types::{
    account_address::AccountAddress,
    effects::{AccountChangeSet, ChangeSet as MoveChangeSet, Op as MoveStorageOp},
    language_storage::{ModuleId, StructTag},
    vm_status::{err_msg, StatusCode, VMStatus},
};
use move_vm_runtime::{move_vm::MoveVM, session::Session};
use serde::{Deserialize, Serialize};
use std::{
    borrow::BorrowMut,
    collections::{BTreeMap, HashMap},
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[derive(BCSCryptoHash, CryptoHasher, Deserialize, Serialize)]
pub enum SessionId {
    Txn {
        sender: AccountAddress,
        sequence_number: u64,
        script_hash: Vec<u8>,
    },
    BlockMeta {
        // block id
        id: HashValue,
    },
    Genesis {
        // id to identify this specific genesis build
        id: HashValue,
    },
    Prologue {
        sender: AccountAddress,
        sequence_number: u64,
        script_hash: Vec<u8>,
    },
    Epilogue {
        sender: AccountAddress,
        sequence_number: u64,
        script_hash: Vec<u8>,
    },
    // For those runs that are not a transaction and the output of which won't be committed.
    Void,
}

impl SessionId {
    pub fn txn(txn: &SignatureCheckedTransaction) -> Self {
        Self::txn_meta(&TransactionMetadata::new(&txn.clone().into_inner()))
    }

    pub fn txn_meta(txn_data: &TransactionMetadata) -> Self {
        Self::Txn {
            sender: txn_data.sender,
            sequence_number: txn_data.sequence_number,
            script_hash: txn_data.script_hash.clone(),
        }
    }

    pub fn genesis(id: HashValue) -> Self {
        Self::Genesis { id }
    }

    pub fn block_meta(block_meta: &BlockMetadata) -> Self {
        Self::BlockMeta {
            id: block_meta.id(),
        }
    }

    pub fn prologue(txn: &SignatureCheckedTransaction) -> Self {
        Self::prologue_meta(&TransactionMetadata::new(&txn.clone().into_inner()))
    }

    pub fn prologue_meta(txn_data: &TransactionMetadata) -> Self {
        Self::Prologue {
            sender: txn_data.sender,
            sequence_number: txn_data.sequence_number,
            script_hash: txn_data.script_hash.clone(),
        }
    }

    pub fn epilogue(txn: &SignatureCheckedTransaction) -> Self {
        Self::epilogue_meta(&TransactionMetadata::new(&txn.clone().into_inner()))
    }

    pub fn epilogue_meta(txn_data: &TransactionMetadata) -> Self {
        Self::Epilogue {
            sender: txn_data.sender,
            sequence_number: txn_data.sequence_number,
            script_hash: txn_data.script_hash.clone(),
        }
    }

    pub fn void() -> Self {
        Self::Void
    }

    pub fn as_uuid(&self) -> HashValue {
        self.hash()
    }
}

pub struct SessionExt<'r, 'l> {
    inner: Session<'r, 'l>,
    remote: &'r dyn AptosMoveResolver,
    features: Arc<Features>,
}

impl<'r, 'l> SessionExt<'r, 'l> {
    pub fn new(
        inner: Session<'r, 'l>,
        remote: &'r dyn AptosMoveResolver,
        features: Arc<Features>,
    ) -> Self {
        Self {
            inner,
            remote,
            features,
        }
    }

    pub fn finish<C: AccessPathCache>(
        self,
        ap_cache: &mut C,
        configs: &ChangeSetConfigs,
    ) -> VMResult<VMChangeSet> {
        let move_vm = self.inner.get_move_vm();
        let (change_set, mut extensions) = self.inner.finish_with_extensions()?;

        let (change_set, resource_group_change_set) =
            Self::split_and_merge_resource_groups(move_vm, self.remote, change_set)?;
        let current_time = CurrentTimeMicroseconds::fetch_config(self.remote);

        let table_context: NativeTableContext = extensions.remove();
        let table_change_set = table_context
            .into_change_set()
            .map_err(|e| e.finish(Location::Undefined))?;

        let aggregator_context: NativeAggregatorContext = extensions.remove();
        let aggregator_change_set = aggregator_context.into_change_set();

        let event_context: NativeEventContext = extensions.remove();
        let events = event_context.into_events();

        let change_set = Self::convert_change_set(
            self.remote,
            self.features.is_storage_slot_metadata_enabled(),
            current_time.as_ref(),
            change_set,
            resource_group_change_set,
            events,
            table_change_set,
            aggregator_change_set,
            ap_cache,
            configs,
        )
        .map_err(|status| PartialVMError::new(status.status_code()).finish(Location::Undefined))?;

        Ok(change_set)
    }

    pub fn extract_publish_request(&mut self) -> Option<PublishRequest> {
        let ctx = self.get_native_extensions().get_mut::<NativeCodeContext>();
        ctx.requested_module_bundle.take()
    }

    /// * Separate the resource groups from the non-resource.
    /// * non-resource groups are kept as is
    /// * resource groups are merged into the correct format as deltas to the source data
    ///   * Remove resource group data from the deltas
    ///   * Attempt to read the existing resource group data or create a new empty container
    ///   * Apply the deltas to the resource group data
    /// The process for translating Move deltas of resource groups to resources is
    /// * Add -- insert element in container
    ///   * If entry exists, Unreachable
    ///   * If group exists, Modify
    ///   * If group doesn't exist, Add
    /// * Modify -- update element in container
    ///   * If group or data doesn't exist, Unreachable
    ///   * Otherwise modify
    /// * Delete -- remove element from container
    ///   * If group or data does't exist, Unreachable
    ///   * If elements remain, Modify
    ///   * Otherwise delete
    fn split_and_merge_resource_groups(
        runtime: &MoveVM,
        remote: &dyn AptosMoveResolver,
        change_set: MoveChangeSet,
    ) -> VMResult<(MoveChangeSet, MoveChangeSet)> {
        // The use of this implies that we could theoretically call unwrap with no consequences,
        // but using unwrap means the code panics if someone can come up with an attack.
        let common_error = || {
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message("split_and_merge_resource_groups error".to_string())
                .finish(Location::Undefined)
        };
        let mut change_set_filtered = MoveChangeSet::new();
        let mut resource_group_change_set = MoveChangeSet::new();

        let mut resource_group_cache = remote.release_resource_group_cache();
        for (addr, account_changeset) in change_set.into_inner() {
            let mut resource_groups: BTreeMap<StructTag, AccountChangeSet> = BTreeMap::new();
            let mut resources_filtered = BTreeMap::new();
            let (modules, resources) = account_changeset.into_inner();

            for (struct_tag, blob_op) in resources {
                let resource_group_tag = runtime
                    .with_module_metadata(&struct_tag.module_id(), |md| {
                        get_resource_group_from_metadata(&struct_tag, md)
                    });

                if let Some(resource_group_tag) = resource_group_tag {
                    resource_groups
                        .entry(resource_group_tag)
                        .or_insert_with(AccountChangeSet::new)
                        .add_resource_op(struct_tag, blob_op)
                        .map_err(|_| common_error())?;
                } else {
                    resources_filtered.insert(struct_tag, blob_op);
                }
            }

            change_set_filtered
                .add_account_changeset(
                    addr,
                    AccountChangeSet::from_modules_resources(modules, resources_filtered),
                )
                .map_err(|_| common_error())?;

            for (resource_group_tag, resources) in resource_groups {
                let mut source_data = resource_group_cache
                    .borrow_mut()
                    .get_mut(&addr)
                    .and_then(|t| t.remove(&resource_group_tag))
                    .unwrap_or_default();
                let create = source_data.is_empty();

                for (struct_tag, current_op) in resources.into_resources() {
                    match current_op {
                        MoveStorageOp::Delete => {
                            source_data.remove(&struct_tag).ok_or_else(common_error)?;
                        },
                        MoveStorageOp::Modify(new_data) => {
                            let data = source_data.get_mut(&struct_tag).ok_or_else(common_error)?;
                            *data = new_data;
                        },
                        MoveStorageOp::New(data) => {
                            let data = source_data.insert(struct_tag, data);
                            if data.is_some() {
                                return Err(common_error());
                            }
                        },
                    }
                }

                let op = if source_data.is_empty() {
                    MoveStorageOp::Delete
                } else if create {
                    MoveStorageOp::New(bcs::to_bytes(&source_data).map_err(|_| common_error())?)
                } else {
                    MoveStorageOp::Modify(bcs::to_bytes(&source_data).map_err(|_| common_error())?)
                };
                resource_group_change_set
                    .add_resource_op(addr, resource_group_tag, op)
                    .map_err(|_| common_error())?;
            }
        }

        Ok((change_set_filtered, resource_group_change_set))
    }

    pub fn convert_change_set<C: AccessPathCache>(
        remote: &dyn AptosMoveResolver,
        is_storage_slot_metadata_enabled: bool,
        current_time: Option<&CurrentTimeMicroseconds>,
        change_set: MoveChangeSet,
        resource_group_change_set: MoveChangeSet,
        events: Vec<ContractEvent>,
        table_change_set: TableChangeSet,
        aggregator_change_set: AggregatorChangeSet,
        ap_cache: &mut C,
        configs: &ChangeSetConfigs,
    ) -> Result<VMChangeSet, VMStatus> {
        let mut new_slot_metadata: Option<StateValueMetadata> = None;
        if is_storage_slot_metadata_enabled {
            if let Some(current_time) = current_time {
                // The deposit on the metadata is a placeholder (0), it will be updated later when
                // storage fee is charged.
                new_slot_metadata = Some(StateValueMetadata::new(0, current_time));
            }
        }
        let woc = WriteOpConverter {
            remote,
            new_slot_metadata,
        };

        let mut resource_write_set = HashMap::new();
        let mut module_write_set = HashMap::new();
        let mut aggregator_write_set = HashMap::new();
        let mut aggregator_delta_set = HashMap::new();

        for (addr, account_changeset) in change_set.into_inner() {
            let (modules, resources) = account_changeset.into_inner();
            for (struct_tag, blob_op) in resources {
                let state_key = StateKey::access_path(ap_cache.get_resource_path(addr, struct_tag));
                let op = woc.convert(
                    &state_key,
                    blob_op,
                    configs.legacy_resource_creation_as_modification(),
                )?;

                resource_write_set.insert(state_key, op);
            }

            for (name, blob_op) in modules {
                let state_key =
                    StateKey::access_path(ap_cache.get_module_path(ModuleId::new(addr, name)));
                let op = woc.convert(&state_key, blob_op, false)?;
                module_write_set.insert(state_key, op);
            }
        }

        for (addr, account_changeset) in resource_group_change_set.into_inner() {
            let (_, resources) = account_changeset.into_inner();
            for (struct_tag, blob_op) in resources {
                let state_key =
                    StateKey::access_path(ap_cache.get_resource_group_path(addr, struct_tag));
                let op = woc.convert(&state_key, blob_op, false)?;
                resource_write_set.insert(state_key, op);
            }
        }

        for (handle, change) in table_change_set.changes {
            for (key, value_op) in change.entries {
                let state_key = StateKey::table_item(handle.into(), key);
                let op = woc.convert(&state_key, value_op, false)?;
                resource_write_set.insert(state_key, op);
            }
        }

        for (id, change) in aggregator_change_set.changes {
            let AggregatorID { handle, key } = id;
            let key_bytes = key.0.to_vec();
            let state_key = StateKey::table_item(handle, key_bytes);

            match change {
                AggregatorChange::Write(value) => {
                    let write_op = woc.convert_aggregator_mod(&state_key, value)?;
                    aggregator_write_set.insert(state_key, write_op);
                },
                AggregatorChange::Merge(delta_op) => {
                    aggregator_delta_set.insert(state_key, delta_op);
                },
                AggregatorChange::Delete => {
                    let write_op = woc.convert(&state_key, MoveStorageOp::Delete, false)?;
                    aggregator_write_set.insert(state_key, write_op);
                },
            }
        }

        VMChangeSet::new(
            resource_write_set,
            module_write_set,
            aggregator_write_set,
            aggregator_delta_set,
            events,
            configs,
        )
    }
}

impl<'r, 'l> Deref for SessionExt<'r, 'l> {
    type Target = Session<'r, 'l>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'r, 'l> DerefMut for SessionExt<'r, 'l> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

struct WriteOpConverter<'r> {
    remote: &'r dyn AptosMoveResolver,
    new_slot_metadata: Option<StateValueMetadata>,
}

impl<'r> WriteOpConverter<'r> {
    fn convert(
        &self,
        state_key: &StateKey,
        move_storage_op: MoveStorageOp<Vec<u8>>,
        legacy_creation_as_modification: bool,
    ) -> Result<WriteOp, VMStatus> {
        use MoveStorageOp::*;
        use WriteOp::*;

        let maybe_existing_metadata =
            self.remote
                .get_state_value_metadata(state_key)
                .map_err(|_| {
                    VMStatus::error(
                        StatusCode::STORAGE_ERROR,
                        err_msg("Storage read failed when converting change set."),
                    )
                })?;

        let write_op = match (maybe_existing_metadata, move_storage_op) {
            (None, Modify(_) | Delete) => {
                return Err(VMStatus::error(
                    // Possible under speculative execution, returning storage error waiting for re-execution
                    StatusCode::STORAGE_ERROR,
                    err_msg("When converting write op: updating non-existent value."),
                ));
            },
            (Some(_), New(_)) => {
                return Err(VMStatus::error(
                    // Possible under speculative execution, returning storage error waiting for re-execution
                    StatusCode::STORAGE_ERROR,
                    err_msg("When converting write op: Recreating existing value."),
                ));
            },
            (None, New(data)) => match &self.new_slot_metadata {
                None => {
                    if legacy_creation_as_modification {
                        Modification(data)
                    } else {
                        Creation(data)
                    }
                },
                Some(metadata) => CreationWithMetadata {
                    data,
                    metadata: metadata.clone(),
                },
            },
            (Some(existing_metadata), Modify(data)) => {
                // Inherit metadata even if the feature flags is turned off, for compatibility.
                match existing_metadata {
                    None => Modification(data),
                    Some(metadata) => ModificationWithMetadata { data, metadata },
                }
            },
            (Some(existing_metadata), Delete) => {
                // Inherit metadata even if the feature flags is turned off, for compatibility.
                match existing_metadata {
                    None => Deletion,
                    Some(metadata) => DeletionWithMetadata { metadata },
                }
            },
        };
        Ok(write_op)
    }

    fn convert_aggregator_mod(
        &self,
        state_key: &StateKey,
        value: u128,
    ) -> Result<WriteOp, VMStatus> {
        let maybe_existing_metadata = self
            .remote
            .get_state_value_metadata(state_key)
            .map_err(|_| VMStatus::error(StatusCode::STORAGE_ERROR, None))?;
        let data = serialize(&value);

        let op = match maybe_existing_metadata {
            None => {
                match &self.new_slot_metadata {
                    // n.b. Aggregator writes historically did not distinguish Create vs Modify.
                    None => WriteOp::Modification(data),
                    Some(metadata) => WriteOp::CreationWithMetadata {
                        data,
                        metadata: metadata.clone(),
                    },
                }
            },
            Some(existing_metadata) => match existing_metadata {
                None => WriteOp::Modification(data),
                Some(metadata) => WriteOp::ModificationWithMetadata { data, metadata },
            },
        };

        Ok(op)
    }
}
