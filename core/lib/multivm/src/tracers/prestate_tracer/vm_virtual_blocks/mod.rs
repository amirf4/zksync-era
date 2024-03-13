use std::{collections::HashMap, sync::Arc};

use once_cell::sync::OnceCell;
use zk_evm_1_3_3::tracing::{BeforeExecutionData, VmLocalStateData};
use zksync_state::{StoragePtr, WriteStorage};
use zksync_types::{
    web3::signing::keccak256, AccountTreeId, StorageKey, StorageValue, H256, L2_ETH_TOKEN_ADDRESS,
};
use zksync_utils::address_to_h256;

use super::{
    get_account_data, process_modified_storage_keys, process_result, PrestateTracer, State,
    StorageAccess,
};
use crate::{
    interface::dyn_tracers::vm_1_3_3::DynTracer,
    tracers::prestate_tracer::U256,
    vm_virtual_blocks::{
        BootloaderState, ExecutionEndTracer, ExecutionProcessing, HistoryMode, SimpleMemory,
        ZkSyncVmState,
    },
};

impl<S: WriteStorage, H: HistoryMode> DynTracer<S, SimpleMemory<H>> for PrestateTracer {
    fn before_execution(
        &mut self,
        _state: VmLocalStateData<'_>,
        _data: BeforeExecutionData,
        _memory: &SimpleMemory<H>,
        storage: StoragePtr<S>,
    ) {
        if self.config.diff_mode {
            self.pre
                .extend(process_modified_storage_keys(self.pre.clone(), &storage));
        }
    }
}

impl<H: HistoryMode> ExecutionEndTracer<H> for PrestateTracer {}

impl<S: WriteStorage, H: HistoryMode> ExecutionProcessing<S, H> for PrestateTracer {
    fn after_vm_execution(
        &mut self,
        state: &mut ZkSyncVmState<S, H>,
        _bootloader_state: &BootloaderState,
        _stop_reason: crate::interface::tracer::VmExecutionStopReason,
    ) {
        let modified_storage_keys = state.storage.storage.inner().get_modified_storage_keys();
        if self.config.diff_mode {
            self.post = modified_storage_keys
                .clone()
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .iter()
                .map(|k| get_account_data(k, state, &modified_storage_keys))
                .collect::<State>();
        } else {
            let read_keys = &state.storage.read_keys;
            let map = read_keys.inner().clone();
            let res = map
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .iter()
                .map(|k| get_account_data(k, state, &modified_storage_keys))
                .collect::<State>();
            self.post = res;
        }
        process_result(&self.result, self.pre.clone(), self.post.clone());
    }
}

impl<S: zksync_state::WriteStorage, H: HistoryMode> StorageAccess for ZkSyncVmState<S, H> {
    fn read_from_storage(&self, key: &StorageKey) -> U256 {
        self.storage.storage.read_from_storage(key)
    }
}
