// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    captured_reads::{
        CapturedReads, DataRead, DelayedFieldRead, DelayedFieldReadKind, GroupRead, ReadKind,
    },
    counters,
    scheduler::{DependencyResult, DependencyStatus, Scheduler, TWaitForDependency},
};
use anyhow::bail;
use aptos_aggregator::{
    bounded_math::{ok_overflow, BoundedMath, SignedU128},
    delta_change_set::serialize,
    delta_math::DeltaHistory,
    resolver::{TAggregatorV1View, TDelayedFieldView},
    types::{
        code_invariant_error, expect_ok, DelayedFieldValue, DelayedFieldsSpeculativeError, PanicOr,
        ReadPosition, TryFromMoveValue, TryIntoMoveValue,
    },
};
use aptos_logger::error;
use aptos_mvhashmap::{
    types::{
        GroupReadResult, MVDataError, MVDataOutput, MVDelayedFieldsError, MVGroupError,
        MVModulesError, MVModulesOutput, StorageVersion, TxnIndex, UnknownOrLayout,
        UnsyncGroupError, ValueWithLayout,
    },
    unsync_map::UnsyncMap,
    versioned_delayed_fields::TVersionedDelayedFieldView,
    MVHashMap,
};
use aptos_state_view::{StateViewId, TStateView};
use aptos_types::{
    aggregator::PanicError,
    executable::{Executable, ModulePath},
    state_store::{
        state_storage_usage::StateStorageUsage,
        state_value::{StateValue, StateValueMetadataKind},
    },
    transaction::BlockExecutableTransaction as Transaction,
    write_set::TransactionWrite,
};
use aptos_vm_logging::{log_schema::AdapterLogSchema, prelude::*};
use aptos_vm_types::resolver::{
    ResourceGroupSize, StateStorageView, TModuleView, TResourceGroupView, TResourceView,
};
use bytes::Bytes;
use claims::assert_ok;
use move_core_types::{
    value::{IdentifierMappingKind, MoveTypeLayout},
    vm_status::{StatusCode, VMStatus},
};
use move_vm_types::{
    value_transformation::{
        deserialize_and_replace_values_with_ids, serialize_and_replace_ids_with_values,
        TransformationError, TransformationResult, ValueToIdentifierMapping,
    },
    values::Value,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

/// A struct which describes the result of the read from the proxy. The client
/// can interpret these types to further resolve the reads.
#[derive(Debug)]
pub(crate) enum ReadResult {
    Value(Option<StateValue>, Option<Arc<MoveTypeLayout>>),
    Metadata(Option<StateValueMetadataKind>),
    Exists(bool),
    Uninitialized,
    // Must halt the execution of the calling transaction. This might be because
    // there was an inconsistency in observed speculative state, or dependency
    // waiting indicated that the parallel execution had been halted. The String
    // parameter provides more context (error description / message).
    HaltSpeculativeExecution(String),
}

impl ReadResult {
    fn from_data_read<V: TransactionWrite>(data: DataRead<V>) -> Self {
        match data {
            DataRead::Versioned(_, v, layout) => ReadResult::Value(v.as_state_value(), layout),
            DataRead::Resolved(v) => {
                // TODO[agg_v1](cleanup): Move AggV1 to Delayed fields, and then handle the layout if needed
                ReadResult::Value(Some(StateValue::new_legacy(serialize(&v).into())), None)
            },
            DataRead::Metadata(maybe_metadata) => ReadResult::Metadata(maybe_metadata),
            DataRead::Exists(exists) => ReadResult::Exists(exists),
        }
    }

    fn from_value_with_layout<V: TransactionWrite>(
        value: ValueWithLayout<V>,
        kind: ReadKind,
    ) -> Option<Self> {
        match (value, kind) {
            (ValueWithLayout::Exchanged(v, layout), ReadKind::Value) => {
                Some(ReadResult::Value(v.as_state_value(), layout))
            },
            (ValueWithLayout::RawFromStorage(_), ReadKind::Value) => None,
            (ValueWithLayout::Exchanged(v, _), ReadKind::Metadata)
            | (ValueWithLayout::RawFromStorage(v), ReadKind::Metadata) => {
                Some(ReadResult::Metadata(v.as_state_value_metadata()))
            },
            (ValueWithLayout::Exchanged(v, _), ReadKind::Exists)
            | (ValueWithLayout::RawFromStorage(v), ReadKind::Exists) => {
                Some(ReadResult::Exists(!v.is_deletion()))
            },
        }
    }

    pub fn into_value(self) -> Option<StateValue> {
        if let ReadResult::Value(v, _layout) = self {
            v
        } else {
            unreachable!("Read result must be Value kind")
        }
    }
}

trait ResourceState<T: Transaction> {
    fn set_base_value(&self, key: T::Key, value: ValueWithLayout<T::Value>);

    fn read_cached_data_by_kind(
        &self,
        txn_idx: TxnIndex,
        key: &T::Key,
        target_kind: ReadKind,
        layout: UnknownOrLayout,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> ReadResult;
}

trait ResourceGroupState<T: Transaction> {
    fn set_raw_group_base_values(&self, group_key: T::Key, base_values: Vec<(T::Tag, T::Value)>);

    fn read_cached_group_tagged_data(
        &self,
        txn_idx: TxnIndex,
        group_key: &T::Key,
        resource_tag: &T::Tag,
        maybe_layout: Option<&MoveTypeLayout>,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> anyhow::Result<GroupReadResult>;
}

pub(crate) struct ParallelState<'a, T: Transaction, X: Executable> {
    versioned_map: &'a MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
    scheduler: &'a Scheduler,
    start_counter: u32,
    counter: &'a AtomicU32,
    captured_reads: RefCell<CapturedReads<T>>,
}

fn get_delayed_field_value_impl<T: Transaction>(
    captured_reads: &RefCell<CapturedReads<T>>,
    versioned_delayed_fields: &dyn TVersionedDelayedFieldView<T::Identifier>,
    wait_for: &dyn TWaitForDependency,
    id: &T::Identifier,
    txn_idx: TxnIndex,
) -> Result<DelayedFieldValue, PanicOr<DelayedFieldsSpeculativeError>> {
    // We expect only DelayedFieldReadKind::Value (which is set from this function),
    // to be a "full materialized/aggregated" read, and so we don't use the value
    // from HistoryBounded reads.
    // If we wanted to make it more dynamic, we could have a type of the read value
    // inside HistoryBounded
    let delayed_read = captured_reads
        .borrow()
        .get_delayed_field_by_kind(id, DelayedFieldReadKind::Value);
    if let Some(data) = delayed_read {
        if let DelayedFieldRead::Value { value, .. } = data {
            return Ok(value);
        } else {
            let err =
                code_invariant_error("Value DelayedField read returned non-value result").into();
            captured_reads
                .borrow_mut()
                .capture_delayed_field_read_error(&err);
            return Err(err);
        }
    }

    loop {
        match versioned_delayed_fields.read(id, txn_idx) {
            Ok(value) => {
                captured_reads.borrow_mut().capture_delayed_field_read(
                    *id,
                    false,
                    DelayedFieldRead::Value {
                        value: value.clone(),
                    },
                )?;
                return Ok(value);
            },
            Err(PanicOr::Or(MVDelayedFieldsError::Dependency(dep_idx))) => {
                if !wait_for_dependency(wait_for, txn_idx, dep_idx) {
                    // TODO[agg_v2](cleanup): think of correct return type
                    return Err(PanicOr::Or(DelayedFieldsSpeculativeError::InconsistentRead));
                }
            },
            Err(e) => {
                captured_reads
                    .borrow_mut()
                    .capture_delayed_field_read_error(&e);
                // TODO[agg_v2](cleanup): think of correct return type
                return Err(e.map_non_panic(|_| DelayedFieldsSpeculativeError::InconsistentRead));
            },
        }
    }
}

fn compute_delayed_field_try_add_delta_outcome_from_history(
    base_delta: &SignedU128,
    delta: &SignedU128,
    max_value: u128,
    mut history: DeltaHistory,
    base_aggregator_value: u128,
) -> Result<(bool, DelayedFieldRead), PanicOr<DelayedFieldsSpeculativeError>> {
    let math = BoundedMath::new(max_value);

    let before_value = expect_ok(math.unsigned_add_delta(base_aggregator_value, base_delta))?;

    let result = if math.unsigned_add_delta(before_value, delta).is_err() {
        match delta {
            SignedU128::Positive(delta_value) => {
                let overflow_delta = expect_ok(ok_overflow(
                    math.unsigned_add_delta(*delta_value, base_delta),
                ))?;

                // We don't need to record the value if it overflowed.
                if let Some(overflow_delta) = overflow_delta {
                    history.record_overflow(overflow_delta);
                }
            },
            SignedU128::Negative(delta_value) => {
                let underflow_delta = expect_ok(ok_overflow(
                    math.unsigned_add_delta(*delta_value, &base_delta.minus()),
                ))?;
                // We don't need to record the value if it overflowed (delta was smaller than -max_value).
                if let Some(underflow_delta) = underflow_delta {
                    history.record_underflow(underflow_delta);
                }
            },
        };

        false
    } else {
        let new_delta = expect_ok(math.signed_add(base_delta, delta))?;
        history.record_success(new_delta);
        true
    };

    Ok((result, DelayedFieldRead::HistoryBounded {
        restriction: history,
        max_value,
        inner_aggregator_value: base_aggregator_value,
    }))
}

fn compute_delayed_field_try_add_delta_outcome_first_time(
    delta: &SignedU128,
    max_value: u128,
    base_aggregator_value: u128,
) -> Result<(bool, DelayedFieldRead), PanicOr<DelayedFieldsSpeculativeError>> {
    let math = BoundedMath::new(max_value);
    let mut history = DeltaHistory::new();
    let result = if math
        .unsigned_add_delta(base_aggregator_value, delta)
        .is_err()
    {
        match delta {
            SignedU128::Positive(delta_value) => {
                history.record_overflow(*delta_value);
            },
            SignedU128::Negative(delta_value) => {
                history.record_underflow(*delta_value);
            },
        };
        false
    } else {
        history.record_success(*delta);
        true
    };

    Ok((result, DelayedFieldRead::HistoryBounded {
        restriction: history,
        max_value,
        inner_aggregator_value: base_aggregator_value,
    }))
}
// TODO[agg_v2](cleanup): see about the split with CapturedReads,
// and whether anything should be moved there.
fn delayed_field_try_add_delta_outcome_impl<T: Transaction>(
    captured_reads: &RefCell<CapturedReads<T>>,
    versioned_delayed_fields: &dyn TVersionedDelayedFieldView<T::Identifier>,
    wait_for: &dyn TWaitForDependency,
    id: &T::Identifier,
    base_delta: &SignedU128,
    delta: &SignedU128,
    max_value: u128,
    txn_idx: TxnIndex,
) -> Result<bool, PanicOr<DelayedFieldsSpeculativeError>> {
    // No need to record or check or try, if input value exceeds the bound.
    if delta.abs() > max_value {
        return Ok(false);
    }

    let delayed_read = captured_reads
        .borrow()
        .get_delayed_field_by_kind(id, DelayedFieldReadKind::HistoryBounded);
    match delayed_read {
        Some(DelayedFieldRead::Value { value }) => {
            let math = BoundedMath::new(max_value);
            let before = expect_ok(
                math.unsigned_add_delta(value.clone().into_aggregator_value()?, base_delta),
            )?;
            Ok(math.unsigned_add_delta(before, delta).is_ok())
        },
        Some(DelayedFieldRead::HistoryBounded {
            restriction: history,
            max_value: before_max_value,
            inner_aggregator_value,
        }) => {
            if before_max_value != max_value {
                return Err(
                    code_invariant_error("Cannot merge deltas with different limits").into(),
                );
            }

            let (result, udpated_delayed_read) =
                compute_delayed_field_try_add_delta_outcome_from_history(
                    base_delta,
                    delta,
                    max_value,
                    history,
                    inner_aggregator_value,
                )?;

            captured_reads.borrow_mut().capture_delayed_field_read(
                *id,
                true,
                udpated_delayed_read,
            )?;
            Ok(result)
        },
        None => {
            if !base_delta.is_zero() {
                return Err(code_invariant_error(
                    "Passed-in delta is not zero, but CapturedReads has no record",
                )
                .into());
            }

            let last_committed_value = loop {
                match versioned_delayed_fields.read_latest_committed_value(
                    id,
                    txn_idx,
                    ReadPosition::BeforeCurrentTxn,
                ) {
                    Ok(v) => break v,
                    Err(MVDelayedFieldsError::Dependency(dep_idx)) => {
                        if !wait_for_dependency(wait_for, txn_idx, dep_idx) {
                            // TODO[agg_v2](cleanup): think of correct return type
                            return Err(PanicOr::Or(
                                DelayedFieldsSpeculativeError::InconsistentRead,
                            ));
                        }
                    },
                    Err(_) => {
                        return Err(PanicOr::Or(DelayedFieldsSpeculativeError::InconsistentRead))
                    },
                };
            }
            .into_aggregator_value()?;

            let (result, new_delayed_read) =
                compute_delayed_field_try_add_delta_outcome_first_time(
                    delta,
                    max_value,
                    last_committed_value,
                )?;

            captured_reads
                .borrow_mut()
                .capture_delayed_field_read(*id, false, new_delayed_read)?;
            Ok(result)
        },
    }
}

// txn_idx is estimated to have a r/w dependency on dep_idx.
// Returns after the dependency has been resolved, the returned indicator is true if
// it is safe to continue, and false if the execution has been halted.
fn wait_for_dependency(
    wait_for: &dyn TWaitForDependency,
    txn_idx: TxnIndex,
    dep_idx: TxnIndex,
) -> bool {
    match wait_for.wait_for_dependency(txn_idx, dep_idx) {
        DependencyResult::Dependency(dep_condition) => {
            let _timer = counters::DEPENDENCY_WAIT_SECONDS.start_timer();
            // Wait on a condition variable corresponding to the encountered
            // read dependency. Once the dep_idx finishes re-execution, scheduler
            // will mark the dependency as resolved, and then the txn_idx will be
            // scheduled for re-execution, which will re-awaken cvar here.
            // A deadlock is not possible due to these condition variables:
            // suppose all threads are waiting on read dependency, and consider
            // one with lowest txn_idx. It observed a dependency, so some thread
            // aborted dep_idx. If that abort returned execution task, by
            // minimality (lower transactions aren't waiting), that thread would
            // finish execution unblock txn_idx, contradiction. Otherwise,
            // execution_idx in scheduler was lower at a time when at least the
            // thread that aborted dep_idx was alive, and again, since lower txns
            // than txn_idx are not blocked, so the execution of dep_idx will
            // eventually finish and lead to unblocking txn_idx, contradiction.
            let (lock, cvar) = &*dep_condition;
            let mut dep_resolved = lock.lock();
            while let DependencyStatus::Unresolved = *dep_resolved {
                dep_resolved = cvar.wait(dep_resolved).unwrap();
            }
            // dep resolved status is either resolved or execution halted.
            matches!(*dep_resolved, DependencyStatus::Resolved)
        },
        DependencyResult::ExecutionHalted => false,
        DependencyResult::Resolved => true,
    }
}

impl<'a, T: Transaction, X: Executable> ParallelState<'a, T, X> {
    pub(crate) fn new(
        shared_map: &'a MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        shared_scheduler: &'a Scheduler,
        start_shared_counter: u32,
        shared_counter: &'a AtomicU32,
    ) -> Self {
        Self {
            versioned_map: shared_map,
            scheduler: shared_scheduler,
            start_counter: start_shared_counter,
            counter: shared_counter,
            captured_reads: RefCell::new(CapturedReads::new()),
        }
    }

    fn set_delayed_field_value(&self, id: T::Identifier, base_value: DelayedFieldValue) {
        self.versioned_map
            .delayed_fields()
            .set_base_value(id, base_value)
    }

    // TODO: Actually fill in the logic to record fetched executables, etc.
    fn fetch_module(
        &self,
        key: &T::Key,
        txn_idx: TxnIndex,
    ) -> anyhow::Result<MVModulesOutput<T::Value, X>, MVModulesError> {
        // Record for the R/W path intersection fallback for modules.
        self.captured_reads
            .borrow_mut()
            .module_reads
            .push(key.clone());

        self.versioned_map.modules().fetch_module(key, txn_idx)
    }

    fn read_group_size(
        &self,
        group_key: &T::Key,
        txn_idx: TxnIndex,
    ) -> anyhow::Result<GroupReadResult> {
        use MVGroupError::*;

        if let Some(group_size) = self.captured_reads.borrow().group_size(group_key) {
            return Ok(GroupReadResult::Size(group_size));
        }

        loop {
            match self
                .versioned_map
                .group_data()
                .get_group_size(group_key, txn_idx)
            {
                Ok(group_size) => {
                    assert_ok!(
                        self.captured_reads
                            .borrow_mut()
                            .capture_group_size(group_key.clone(), group_size),
                        "Group size may not be inconsistent: must be recorded once"
                    );

                    return Ok(GroupReadResult::Size(group_size));
                },
                Err(Uninitialized) => {
                    return Ok(GroupReadResult::Uninitialized);
                },
                Err(TagNotFound) => {
                    unreachable!("Reading group size does not require a specific tag look-up");
                },
                Err(Dependency(dep_idx)) => {
                    if !wait_for_dependency(self.scheduler, txn_idx, dep_idx) {
                        bail!("Interrupted as block execution was halted");
                    }
                },
                Err(TagSerializationError) => {
                    bail!("Resource tag bcs serialization error");
                },
            }
        }
    }
}

impl<'a, T: Transaction, X: Executable> ResourceState<T> for ParallelState<'a, T, X> {
    fn set_base_value(&self, key: T::Key, value: ValueWithLayout<T::Value>) {
        self.versioned_map.data().set_base_value(key, value);
    }

    /// Captures a read from the VM execution, but not unresolved deltas, as in this case it is the
    /// callers responsibility to set the aggregator's base value and call fetch_data again.
    fn read_cached_data_by_kind(
        &self,
        txn_idx: TxnIndex,
        key: &T::Key,
        target_kind: ReadKind,
        layout: UnknownOrLayout,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> ReadResult {
        use MVDataError::*;
        use MVDataOutput::*;

        if let Some(data) = self
            .captured_reads
            .borrow()
            .get_by_kind(key, None, target_kind.clone())
        {
            return ReadResult::from_data_read(data);
        }

        loop {
            match self.versioned_map.data().fetch_data(key, txn_idx) {
                Ok(Versioned(version, value)) => {
                    // If we have a known layout, upgrade RawFromStorage value to Exchanged.
                    if let UnknownOrLayout::Known(layout) = layout {
                        if let ValueWithLayout::RawFromStorage(v) = value {
                            assert_eq!(version, Err(StorageVersion), "Fetched resource has unknown layout but the version is not Err(StorageVersion)");
                            match patch_base_value(v.as_ref(), layout) {
                                Ok(patched_value) => {
                                    self.versioned_map.data().set_base_value(
                                        key.clone(),
                                        ValueWithLayout::Exchanged(
                                            Arc::new(patched_value),
                                            layout.cloned().map(Arc::new),
                                        ),
                                    );
                                    // Refetch in case a concurrent change went through.
                                    continue;
                                },
                                Err(e) => {
                                    error!("Couldn't patch value from versioned map: {}", e);
                                    self.captured_reads.borrow_mut().mark_incorrect_use();
                                    return ReadResult::HaltSpeculativeExecution(
                                        "Couldn't patch value from versioned map".to_string(),
                                    );
                                },
                            }
                        }
                    }

                    let data_read = match DataRead::from_value_with_layout(version, value)
                        .downcast(target_kind)
                    {
                        Some(data_read) => data_read,
                        None => {
                            error!("Couldn't downcast value from versioned map");
                            self.captured_reads.borrow_mut().mark_incorrect_use();
                            return ReadResult::HaltSpeculativeExecution(
                                "Couldn't downcast value from versioned map".to_string(),
                            );
                        },
                    };

                    if self
                        .captured_reads
                        .borrow_mut()
                        .capture_read(key.clone(), None, data_read.clone())
                        .is_err()
                    {
                        // Inconsistency in recorded reads.
                        return ReadResult::HaltSpeculativeExecution(
                            "Inconsistency in reads (must be due to speculation)".to_string(),
                        );
                    }

                    return ReadResult::from_data_read(data_read);
                },
                Ok(Resolved(value)) => {
                    let data_read = DataRead::Resolved(value)
                        .downcast(target_kind)
                        .expect("Downcast from Resolved must succeed");

                    if self
                        .captured_reads
                        .borrow_mut()
                        .capture_read(key.clone(), None, data_read.clone())
                        .is_err()
                    {
                        // Inconsistency in recorded reads.
                        return ReadResult::HaltSpeculativeExecution(
                            "Inconsistency in reads (must be due to speculation)".to_string(),
                        );
                    }

                    return ReadResult::from_data_read(data_read);
                },
                Err(Uninitialized) | Err(Unresolved(_)) => {
                    // The underlying assumption here for not recording anything about the read is
                    // that the caller is expected to initialize the contents and serve the reads
                    // solely via the 'fetch_read' interface. Thus, the later, successful read,
                    // will make the needed recordings.
                    return ReadResult::Uninitialized;
                },
                Err(Dependency(dep_idx)) => {
                    if !wait_for_dependency(self.scheduler, txn_idx, dep_idx) {
                        return ReadResult::HaltSpeculativeExecution(
                            "Interrupted as block execution was halted".to_string(),
                        );
                    }
                },
                Err(DeltaApplicationFailure) => {
                    // AggregatorV1 may have delta application failure due to speculation.
                    self.captured_reads.borrow_mut().mark_failure();
                    return ReadResult::HaltSpeculativeExecution(
                        "Delta application failure (must be speculative)".to_string(),
                    );
                },
            };
        }
    }
}

impl<'a, T: Transaction, X: Executable> ResourceGroupState<T> for ParallelState<'a, T, X> {
    fn set_raw_group_base_values(&self, group_key: T::Key, base_values: Vec<(T::Tag, T::Value)>) {
        self.versioned_map
            .group_data()
            .set_raw_base_values(group_key.clone(), base_values);
    }

    fn read_cached_group_tagged_data(
        &self,
        txn_idx: TxnIndex,
        group_key: &T::Key,
        resource_tag: &T::Tag,
        maybe_layout: Option<&MoveTypeLayout>,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> anyhow::Result<GroupReadResult> {
        use MVGroupError::*;

        if let Some(DataRead::Versioned(_, v, layout)) =
            self.captured_reads
                .borrow()
                .get_by_kind(group_key, Some(resource_tag), ReadKind::Value)
        {
            return Ok(GroupReadResult::Value(v.extract_raw_bytes(), layout));
        }

        loop {
            match self.versioned_map.group_data().fetch_tagged_data(
                group_key,
                resource_tag,
                txn_idx,
            ) {
                Ok((version, value_with_layout)) => {
                    // If we have a known layout, upgrade RawFromStorage value to Exchanged.
                    match value_with_layout {
                        ValueWithLayout::RawFromStorage(v) => {
                            let patched_value = patch_base_value(v.as_ref(), maybe_layout)?;
                            self.versioned_map
                                .group_data()
                                .update_tagged_base_value_with_layout(
                                    group_key.clone(),
                                    resource_tag.clone(),
                                    patched_value,
                                    maybe_layout.cloned().map(Arc::new),
                                );
                            // Refetch in case a concurrent change went through.
                            continue;
                        },
                        ValueWithLayout::Exchanged(value, layout) => {
                            let data_read =
                                DataRead::Versioned(version, value.clone(), layout.clone());
                            assert_ok!(
                                self.captured_reads.borrow_mut().capture_read(
                                    group_key.clone(),
                                    Some(resource_tag.clone()),
                                    data_read
                                ),
                                "Resource read in group recorded once: may not be inconsistent"
                            );
                            return Ok(GroupReadResult::Value(
                                value.extract_raw_bytes(),
                                layout.clone(),
                            ));
                        },
                    }
                },
                Err(Uninitialized) => {
                    return Ok(GroupReadResult::Uninitialized);
                },
                Err(TagNotFound) => {
                    let data_read = DataRead::Versioned(
                        Err(StorageVersion),
                        Arc::<T::Value>::new(TransactionWrite::from_state_value(None)),
                        None,
                    );
                    assert_ok!(
                        self.captured_reads.borrow_mut().capture_read(
                            group_key.clone(),
                            Some(resource_tag.clone()),
                            data_read
                        ),
                        "Resource read in group recorded once: may not be inconsistent"
                    );

                    return Ok(GroupReadResult::Value(None, None));
                },
                Err(Dependency(dep_idx)) => {
                    if !wait_for_dependency(self.scheduler, txn_idx, dep_idx) {
                        bail!("Interrupted as block execution was halted");
                    }
                },
                Err(TagSerializationError) => {
                    unreachable!("Reading a resource does not require tag serialization");
                },
            }
        }
    }
}

pub(crate) struct SequentialState<'a, T: Transaction, X: Executable> {
    pub(crate) unsync_map: &'a UnsyncMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
    pub(crate) resource_with_layout_read_set: RefCell<HashSet<T::Key>>,
    pub(crate) group_read_set: RefCell<HashSet<T::Key>>,
    pub(crate) start_counter: u32,
    pub(crate) counter: &'a RefCell<u32>,
    pub(crate) dynamic_change_set_optimizations_enabled: bool,
    pub(crate) incorrect_use: RefCell<bool>,
}

impl<'a, T: Transaction, X: Executable> SequentialState<'a, T, X> {
    pub fn new(
        unsync_map: &'a UnsyncMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        start_counter: u32,
        counter: &'a RefCell<u32>,
        dynamic_change_set_optimizations_enabled: bool,
    ) -> Self {
        Self {
            unsync_map,
            resource_with_layout_read_set: RefCell::new(HashSet::new()),
            group_read_set: RefCell::new(HashSet::new()),
            start_counter,
            counter,
            dynamic_change_set_optimizations_enabled,
            incorrect_use: RefCell::new(false),
        }
    }

    fn set_delayed_field_value(&self, id: T::Identifier, base_value: DelayedFieldValue) {
        self.unsync_map.write_delayed_field(id, base_value)
    }

    fn read_delayed_field(&self, id: T::Identifier) -> Option<DelayedFieldValue> {
        self.unsync_map.fetch_delayed_field(&id)
    }
}

impl<'a, T: Transaction, X: Executable> ResourceState<T> for SequentialState<'a, T, X> {
    fn set_base_value(&self, key: T::Key, value: ValueWithLayout<T::Value>) {
        self.unsync_map.set_base_value(key, value);
    }

    fn read_cached_data_by_kind(
        &self,
        _txn_idx: TxnIndex,
        key: &T::Key,
        target_kind: ReadKind,
        layout: UnknownOrLayout,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> ReadResult {
        match self.unsync_map.fetch_data(key) {
            Some(mut value) => {
                // If we have a known layout, upgrade RawFromStorage value to Exchanged.
                if let UnknownOrLayout::Known(layout) = layout {
                    if let ValueWithLayout::RawFromStorage(v) = value {
                        match patch_base_value(v.as_ref(), layout) {
                            Ok(patched_value) => {
                                let exchanged_value = ValueWithLayout::Exchanged(
                                    Arc::new(patched_value.clone()),
                                    layout.cloned().map(Arc::new),
                                );
                                self.unsync_map
                                    .set_base_value(key.clone(), exchanged_value.clone());

                                // sequential execution doesn't need to worry about concurrent change going through.
                                value = exchanged_value;
                            },
                            Err(_) => {
                                *self.incorrect_use.borrow_mut() = true;
                                error!("Unsync map couldn't patch base value");
                                return ReadResult::HaltSpeculativeExecution(
                                    "Unsync map couldn't patch base value".to_string(),
                                );
                            },
                        }
                    }
                }

                if let Some(ret) = ReadResult::from_value_with_layout(value, target_kind.clone()) {
                    if target_kind == ReadKind::Value {
                        if let ReadResult::Value(v, l) = &ret {
                            if v.is_some() && l.is_some() {
                                self.resource_with_layout_read_set
                                    .borrow_mut()
                                    .insert(key.clone());
                            }
                        }
                    }
                    ret
                } else {
                    *self.incorrect_use.borrow_mut() = true;
                    error!(
                        "Unsync map has RawFromStorage value type, while we are requesting value"
                    );
                    ReadResult::HaltSpeculativeExecution(
                        "Unsync map has RawFromStorage value type, while we are requesting value"
                            .to_string(),
                    )
                }
            },
            None => ReadResult::Uninitialized,
        }
    }
}

impl<'a, T: Transaction, X: Executable> ResourceGroupState<T> for SequentialState<'a, T, X> {
    fn set_raw_group_base_values(&self, group_key: T::Key, base_values: Vec<(T::Tag, T::Value)>) {
        self.unsync_map
            .set_group_base_values(group_key.clone(), base_values);
    }

    fn read_cached_group_tagged_data(
        &self,
        _txn_idx: TxnIndex,
        group_key: &T::Key,
        resource_tag: &T::Tag,
        maybe_layout: Option<&MoveTypeLayout>,
        patch_base_value: &dyn Fn(&T::Value, Option<&MoveTypeLayout>) -> anyhow::Result<T::Value>,
    ) -> anyhow::Result<GroupReadResult> {
        match self
            .unsync_map
            .fetch_group_tagged_data(group_key, resource_tag)
        {
            Ok(mut value) => {
                // If we have a known layout, upgrade RawFromStorage value to Exchanged.
                if let ValueWithLayout::RawFromStorage(v) = value {
                    let patched_value = patch_base_value(v.as_ref(), maybe_layout)?;
                    let maybe_layout = maybe_layout.cloned().map(Arc::new);
                    self.unsync_map.update_tagged_base_value_with_layout(
                        group_key.clone(),
                        resource_tag.clone(),
                        patched_value.clone(),
                        maybe_layout.clone(),
                    );

                    // sequential execution doesn't need to worry about concurrent change going through.
                    value = ValueWithLayout::Exchanged(Arc::new(patched_value), maybe_layout);
                }

                if let ValueWithLayout::Exchanged(v, l) = value {
                    let bytes = v.extract_raw_bytes();
                    if bytes.is_some() && l.is_some() {
                        self.group_read_set.borrow_mut().insert(group_key.clone());
                    }
                    Ok(GroupReadResult::Value(bytes, l.clone()))
                } else {
                    *self.incorrect_use.borrow_mut() = true;
                    error!(
                        "Unsync map has RawFromStorage value type, while we are requesting value"
                    );
                    Ok(GroupReadResult::Uninitialized)
                }
            },
            Err(UnsyncGroupError::Uninitialized) => Ok(GroupReadResult::Uninitialized),
            Err(UnsyncGroupError::TagNotFound) => Ok(GroupReadResult::Value(None, None)),
        }
    }
}

pub(crate) enum ViewState<'a, T: Transaction, X: Executable> {
    Sync(ParallelState<'a, T, X>),
    Unsync(SequentialState<'a, T, X>),
}

impl<'a, T: Transaction, X: Executable> ViewState<'a, T, X> {
    fn get_resource_state(&self) -> &dyn ResourceState<T> {
        match self {
            ViewState::Sync(state) => state,
            ViewState::Unsync(state) => state,
        }
    }

    fn get_resource_group_state(&self) -> &dyn ResourceGroupState<T> {
        match self {
            ViewState::Sync(state) => state,
            ViewState::Unsync(state) => state,
        }
    }
}

/// A struct that represents a single block execution worker thread's view into the state,
/// some of which (in Sync case) might be shared with other workers / threads. By implementing
/// all necessary traits, LatestView is provided to the VM and used to intercept the reads.
/// In the Sync case, also records captured reads for later validation. latest_txn_idx
/// must be set according to the latest transaction that the worker was / is executing.
pub(crate) struct LatestView<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> {
    base_view: &'a S,
    latest_view: ViewState<'a, T, X>,
    txn_idx: TxnIndex,
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> LatestView<'a, T, S, X> {
    pub(crate) fn new(
        base_view: &'a S,
        latest_view: ViewState<'a, T, X>,
        txn_idx: TxnIndex,
    ) -> Self {
        Self {
            base_view,
            latest_view,
            txn_idx,
        }
    }

    #[cfg(test)]
    fn get_resource_with_layout_read_set_sequential(&self) -> HashSet<T::Key> {
        match &self.latest_view {
            ViewState::Sync(_) => {
                unreachable!("Get resource read set called in parallel setting")
            },
            ViewState::Unsync(state) => state.resource_with_layout_read_set.borrow().clone(),
        }
    }

    /// Drains the captured reads.
    pub(crate) fn take_reads(&self) -> CapturedReads<T> {
        match &self.latest_view {
            ViewState::Sync(state) => state.captured_reads.take(),
            ViewState::Unsync(_) => {
                unreachable!("Take reads called in sequential setting (not captured)")
            },
        }
    }

    fn mark_incorrect_use(&self) {
        match &self.latest_view {
            ViewState::Sync(state) => state.captured_reads.borrow_mut().mark_incorrect_use(),
            ViewState::Unsync(state) => *state.incorrect_use.borrow_mut() = true,
        }
    }

    pub fn is_incorrect_use(&self) -> bool {
        match &self.latest_view {
            ViewState::Sync(state) => state.captured_reads.borrow().is_incorrect_use(),
            ViewState::Unsync(state) => *state.incorrect_use.borrow(),
        }
    }

    fn get_raw_base_value(&self, state_key: &T::Key) -> anyhow::Result<Option<StateValue>> {
        let ret = self.base_view.get_state_value(state_key);

        if ret.is_err() {
            // Even speculatively, reading from base view should not return an error.
            // Thus, this critical error log and count does not need to be buffered.
            let log_context = AdapterLogSchema::new(self.base_view.id(), self.txn_idx as usize);
            alert!(
                log_context,
                "[VM, StateView] Error getting data from storage for {:?}",
                state_key
            );
            self.mark_incorrect_use();
        }

        ret
    }

    fn patch_base_value(
        &self,
        value: &T::Value,
        layout: Option<&MoveTypeLayout>,
    ) -> anyhow::Result<T::Value> {
        let maybe_patched = match (value.as_state_value(), layout) {
            (Some(state_value), Some(layout)) => {
                let res = self.replace_values_with_identifiers(state_value, layout);
                match res {
                    Ok((value, _)) => Some(value),
                    Err(err) => {
                        let log_context =
                            AdapterLogSchema::new(self.base_view.id(), self.txn_idx as usize);
                        alert!(
                            log_context,
                            "[VM, ResourceView] Error during value to id replacement: {}",
                            err
                        );
                        self.mark_incorrect_use();
                        return Err(err);
                    },
                }
            },
            (state_value, _) => state_value,
        };
        Ok(TransactionWrite::from_state_value(maybe_patched))
    }

    fn get_base_value_with_layout(
        &self,
        state_key: &T::Key,
        layout: UnknownOrLayout,
    ) -> anyhow::Result<ValueWithLayout<T::Value>> {
        let state_value = self.get_raw_base_value(state_key)?;
        let value = TransactionWrite::from_state_value(state_value);
        Ok(match layout {
            UnknownOrLayout::Known(l) => {
                let layout_arc = l.cloned().map(Arc::new);
                ValueWithLayout::Exchanged(Arc::new(self.patch_base_value(&value, l)?), layout_arc)
            },
            UnknownOrLayout::Unknown => ValueWithLayout::RawFromStorage(Arc::new(value)),
        })
    }

    /// Given a state value, performs deserialization-serialization round-trip
    /// to replace any aggregator / snapshot values.
    fn replace_values_with_identifiers(
        &self,
        state_value: StateValue,
        layout: &MoveTypeLayout,
    ) -> anyhow::Result<(StateValue, HashSet<T::Identifier>)> {
        let mapping = TemporaryValueToIdentifierMapping::new(self, self.txn_idx);
        state_value
            .map_bytes(|bytes| {
                // This call will replace all occurrences of aggregator / snapshot
                // values with unique identifiers with the same type layout.
                // The values are stored in aggregators multi-version data structure,
                // see the actual trait implementation for more details.
                let patched_value =
                    deserialize_and_replace_values_with_ids(bytes.as_ref(), layout, &mapping)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Failed to deserialize resource during id replacement")
                        })?;
                patched_value
                    .simple_serialize(layout)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Failed to serialize value {} after id replacement",
                            patched_value
                        )
                    })
                    .map(|b| b.into())
            })
            .map(|v| (v, mapping.into_inner()))
    }

    /// Given a state value, performs deserialization-serialization round-trip
    /// to replace any aggregator / snapshot values.
    pub(crate) fn replace_identifiers_with_values(
        &self,
        bytes: &Bytes,
        layout: &MoveTypeLayout,
    ) -> anyhow::Result<(Bytes, HashSet<T::Identifier>)> {
        // This call will replace all occurrences of aggregator / snapshot
        // identifiers with values with the same type layout.
        let value = Value::simple_deserialize(bytes, layout).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to deserialize resource during id replacement: {:?}",
                bytes
            )
        })?;
        let mapping = TemporaryValueToIdentifierMapping::new(self, self.txn_idx);
        let patched_bytes = serialize_and_replace_ids_with_values(&value, layout, &mapping)
            .ok_or_else(|| anyhow::anyhow!("Failed to serialize resource during id replacement"))?
            .into();
        Ok((patched_bytes, mapping.into_inner()))
    }

    // Given a bytes, where values were already exchanged with idnetifiers,
    // return a list of identifiers present in it.
    fn extract_identifiers_from_value(
        &self,
        bytes: &Bytes,
        layout: &MoveTypeLayout,
    ) -> anyhow::Result<HashSet<T::Identifier>> {
        let mapping = TemporaryExtractIdentifiersMapping::<T>::new();
        // TODO[agg_v2](cleanup) rename deserialize_and_replace_values_with_ids to not be specific to mapping trait implementation
        // TODO[agg_v2](cleanup) provide traversal method, that doesn't create unnecessary patched value.
        let _patched_value =
            deserialize_and_replace_values_with_ids(bytes.as_ref(), layout, &mapping).ok_or_else(
                || anyhow::anyhow!("Failed to deserialize resource during id replacement"),
            )?;
        Ok(mapping.into_inner())
    }

    fn does_value_need_exchange(
        &self,
        value: &T::Value,
        layout: &Arc<MoveTypeLayout>,
        delayed_write_set_keys: &HashSet<T::Identifier>,
        key: &T::Key,
    ) -> Option<Result<(T::Key, (T::Value, Arc<MoveTypeLayout>)), PanicError>> {
        if let Some(bytes) = value.bytes() {
            let identifiers_in_read_result = self.extract_identifiers_from_value(bytes, layout);

            match identifiers_in_read_result {
                Ok(identifiers_in_read) => {
                    if !delayed_write_set_keys.is_disjoint(&identifiers_in_read) {
                        return Some(Ok((key.clone(), (value.clone(), layout.clone()))));
                    }
                },
                Err(e) => {
                    return Some(Err(code_invariant_error(format!("Cannot extract identifiers from value that identifiers were exchanged into before {:?}", e))))
                }
            }
        }
        None
    }

    fn get_reads_needing_exchange_parallel(
        &self,
        read_set: &CapturedReads<T>,
        delayed_write_set_keys: &HashSet<T::Identifier>,
        skip: &HashSet<T::Key>,
    ) -> Result<BTreeMap<T::Key, (T::Value, Arc<MoveTypeLayout>)>, PanicError> {
        read_set
            .get_read_values_with_delayed_fields()
            .filter(|(key, _)| !skip.contains(key))
            .flat_map(|(key, data_read)| {
                if let DataRead::Versioned(_version, value, Some(layout)) = data_read {
                    return self.does_value_need_exchange(
                        value,
                        layout,
                        delayed_write_set_keys,
                        key,
                    );
                }
                None
            })
            .collect()
    }

    fn get_reads_needing_exchange_sequential(
        &self,
        read_set: &HashSet<T::Key>,
        unsync_map: &UnsyncMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        delayed_write_set_keys: &HashSet<T::Identifier>,
        skip: &HashSet<T::Key>,
    ) -> Result<BTreeMap<T::Key, (T::Value, Arc<MoveTypeLayout>)>, PanicError> {
        read_set
            .iter()
            .filter(|key| !skip.contains(key))
            .flat_map(|key| match unsync_map.fetch_data(key) {
                Some(ValueWithLayout::Exchanged(value, Some(layout))) => self
                    .does_value_need_exchange(value.as_ref(), &layout, delayed_write_set_keys, key),
                Some(ValueWithLayout::Exchanged(_, None)) => None,
                Some(ValueWithLayout::RawFromStorage(_)) => Some(Err(code_invariant_error(
                    "Cannot exchange value that was not exchanged before",
                ))),
                None => None,
            })
            .collect()
    }

    fn get_group_reads_needing_exchange_parallel(
        &self,
        parallel_state: &ParallelState<'a, T, X>,
        delayed_write_set_keys: &HashSet<T::Identifier>,
        skip: &HashSet<T::Key>,
    ) -> Result<BTreeMap<T::Key, (T::Value, u64)>, PanicError> {
        let reads_with_delayed_fields = parallel_state
            .captured_reads
            .borrow()
            .get_group_read_values_with_delayed_fields(skip)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();

        reads_with_delayed_fields
            .into_iter()
            .flat_map(|(key, group_read)| {
                let GroupRead { inner_reads, .. } = group_read;
                let mut resources_needing_delayed_field_exchange = false;
                for data_read in inner_reads.values() {
                    if let DataRead::Versioned(_version, value, Some(layout)) = data_read {
                        if let Some(bytes) = value.bytes() {
                            let identifiers_in_read = self
                                .extract_identifiers_from_value(bytes, layout.as_ref())
                                .unwrap();
                            if !delayed_write_set_keys.is_disjoint(&identifiers_in_read) {
                                // TODO[agg_v2](optimize): Is it possible to avoid clones here?
                                resources_needing_delayed_field_exchange = true;
                                break;
                            }
                        }
                    }
                }
                if !resources_needing_delayed_field_exchange {
                    return None;
                }

                if let Ok(Some(maybe_metadata)) = self.get_resource_state_value_metadata(&key) {
                    let maybe_metadata = maybe_metadata.map(
                        |metadata: aptos_types::state_store::state_value::StateValueMetadata| {
                            StateValue::new_with_metadata(Bytes::new(), metadata)
                        },
                    );
                    if let Ok(GroupReadResult::Size(group_size)) =
                        parallel_state.read_group_size(&key, self.txn_idx)
                    {
                        let metadata_op: T::Value =
                            TransactionWrite::from_state_value(maybe_metadata);
                        if let Some(metadata_op) = metadata_op.convert_read_to_modification() {
                            return Some(Ok((key.clone(), (metadata_op, group_size.get()))));
                        }
                    } else {
                        return Some(Err(code_invariant_error(format!(
                            "Cannot compute metadata op size for the group read {:?}",
                            key
                        ))));
                    }
                }
                Some(Err(code_invariant_error(format!(
                    "Cannot compute metadata op for the group read {:?}",
                    key
                ))))
            })
            .collect()
    }

    fn get_group_reads_needing_exchange_sequential(
        &self,
        group_read_set: &HashSet<T::Key>,
        unsync_map: &UnsyncMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        delayed_write_set_keys: &HashSet<T::Identifier>,
        skip: &HashSet<T::Key>,
    ) -> Result<BTreeMap<T::Key, (T::Value, u64)>, PanicError> {
        group_read_set
            .iter()
            .filter(|key| !skip.contains(key))
            .flat_map(|key| {
                if let Some(value_vec) = unsync_map.fetch_group_data(key) {
                    let mut resources_needing_delayed_field_exchange = false;
                    for (_, value_with_layout) in value_vec {
                        if let ValueWithLayout::Exchanged(value, Some(layout)) = value_with_layout {
                            if let Some(bytes) = value.bytes() {
                                let identifiers_in_read = self
                                    .extract_identifiers_from_value(bytes, layout.as_ref())
                                    .unwrap();
                                if !delayed_write_set_keys.is_disjoint(&identifiers_in_read) {
                                    resources_needing_delayed_field_exchange = true;
                                    break;
                                }
                            }
                        }
                    }
                    if !resources_needing_delayed_field_exchange {
                        return None;
                    }
                    if let Some(metadata) = unsync_map.fetch_data(key) {
                        if let Ok(GroupReadResult::Size(group_size)) =
                            unsync_map.get_group_size(key)
                        {
                            if let Some(metadata_op) = metadata
                                .extract_value_no_layout()
                                .clone()
                                .convert_read_to_modification()
                            {
                                return Some(Ok((key.clone(), (metadata_op, group_size.get()))));
                            }
                        } else {
                            return Some(Err(code_invariant_error(format!(
                                "Cannot compute metadata op size for the group read {:?}",
                                key
                            ))));
                        }
                    }
                    return Some(Err(code_invariant_error(format!(
                        "Cannot compute metadata op for the group read {:?}",
                        key
                    ))));
                }
                None
            })
            .collect()
    }

    fn get_resource_state_value_impl(
        &self,
        state_key: &T::Key,
        layout: UnknownOrLayout,
        kind: ReadKind,
    ) -> anyhow::Result<ReadResult> {
        debug_assert!(
            state_key.module_path().is_none(),
            "Reading a module {:?} using ResourceView",
            state_key,
        );

        let layout = if self.is_delayed_field_optimization_capable() {
            layout
        } else {
            match layout {
                UnknownOrLayout::Known(_) => UnknownOrLayout::Known(None),
                UnknownOrLayout::Unknown => UnknownOrLayout::Unknown,
            }
        };

        let state = self.latest_view.get_resource_state();

        let mut ret = state.read_cached_data_by_kind(
            self.txn_idx,
            state_key,
            kind.clone(),
            layout.clone(),
            &|value, layout| self.patch_base_value(value, layout),
        );
        if matches!(ret, ReadResult::Uninitialized) {
            let from_storage = self.get_base_value_with_layout(state_key, layout.clone())?;
            state.set_base_value(state_key.clone(), from_storage);

            // In case of concurrent storage fetches, we cannot use our value,
            // but need to fetch it from versioned_map again.
            ret = state.read_cached_data_by_kind(
                self.txn_idx,
                state_key,
                kind,
                layout.clone(),
                &|value, layout| self.patch_base_value(value, layout),
            );
        }

        match ret {
            // ExecutionHalted indicates that the parallel execution is halted.
            // The read should return immediately and log the error.
            // For now we use SPECULATIVE_EXECUTION_ABORT_ERROR as the VM
            // will not log the speculative error,
            // so no actual error will be logged once the execution is halted and
            // the speculative logging is flushed.
            ReadResult::HaltSpeculativeExecution(msg) => Err(anyhow::Error::new(VMStatus::error(
                StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR,
                Some(msg),
            ))),
            ReadResult::Uninitialized => {
                unreachable!("base value must already be recorded in the MV data structure")
            },
            _ => Ok(ret),
        }
    }

    fn initialize_mvhashmap_base_group_contents(&self, group_key: &T::Key) -> anyhow::Result<()> {
        let (base_group, metadata_op): (BTreeMap<T::Tag, Bytes>, _) =
            match self.get_raw_base_value(group_key)? {
                Some(state_value) => (
                    bcs::from_bytes(state_value.bytes())
                        .map_err(|_| anyhow::Error::msg("Resource group deserialization error"))?,
                    TransactionWrite::from_state_value(Some(state_value)),
                ),
                None => (BTreeMap::new(), TransactionWrite::from_state_value(None)),
            };
        let base_group_sentinel_ops = base_group
            .into_iter()
            .map(|(t, bytes)| {
                (
                    t,
                    TransactionWrite::from_state_value(Some(StateValue::new_legacy(bytes))),
                )
            })
            .collect();

        self.latest_view
            .get_resource_group_state()
            .set_raw_group_base_values(group_key.clone(), base_group_sentinel_ops);
        self.latest_view.get_resource_state().set_base_value(
            group_key.clone(),
            ValueWithLayout::RawFromStorage(Arc::new(metadata_op)),
        );
        Ok(())
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> TResourceView
    for LatestView<'a, T, S, X>
{
    type Key = T::Key;
    type Layout = MoveTypeLayout;

    fn get_resource_state_value(
        &self,
        state_key: &Self::Key,
        maybe_layout: Option<&Self::Layout>,
    ) -> anyhow::Result<Option<StateValue>> {
        self.get_resource_state_value_impl(
            state_key,
            UnknownOrLayout::Known(maybe_layout),
            ReadKind::Value,
        )
        .map(|res| res.into_value())
    }

    fn get_resource_state_value_metadata(
        &self,
        state_key: &Self::Key,
    ) -> anyhow::Result<Option<StateValueMetadataKind>> {
        self.get_resource_state_value_impl(state_key, UnknownOrLayout::Unknown, ReadKind::Metadata)
            .map(|res| {
                if let ReadResult::Metadata(v) = res {
                    v
                } else {
                    unreachable!("Read result must be Metadata kind")
                }
            })
    }

    fn resource_exists(&self, state_key: &Self::Key) -> anyhow::Result<bool> {
        self.get_resource_state_value_impl(state_key, UnknownOrLayout::Unknown, ReadKind::Exists)
            .map(|res| {
                if let ReadResult::Exists(v) = res {
                    v
                } else {
                    unreachable!("Read result must be Exists kind")
                }
            })
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> TResourceGroupView
    for LatestView<'a, T, S, X>
{
    type GroupKey = T::Key;
    type Layout = MoveTypeLayout;
    type ResourceTag = T::Tag;

    fn resource_group_size(&self, group_key: &Self::GroupKey) -> anyhow::Result<ResourceGroupSize> {
        let mut group_read = match &self.latest_view {
            ViewState::Sync(state) => state.read_group_size(group_key, self.txn_idx)?,
            ViewState::Unsync(state) => state.unsync_map.get_group_size(group_key)?,
        };

        if matches!(group_read, GroupReadResult::Uninitialized) {
            self.initialize_mvhashmap_base_group_contents(group_key)?;

            group_read = match &self.latest_view {
                ViewState::Sync(state) => state.read_group_size(group_key, self.txn_idx)?,
                ViewState::Unsync(state) => state.unsync_map.get_group_size(group_key)?,
            }
        };

        Ok(group_read.into_size())
    }

    fn get_resource_from_group(
        &self,
        group_key: &Self::GroupKey,
        resource_tag: &Self::ResourceTag,
        maybe_layout: Option<&Self::Layout>,
    ) -> anyhow::Result<Option<Bytes>> {
        let maybe_layout = maybe_layout.filter(|_| self.is_delayed_field_optimization_capable());

        let mut group_read = self
            .latest_view
            .get_resource_group_state()
            .read_cached_group_tagged_data(
                self.txn_idx,
                group_key,
                resource_tag,
                maybe_layout,
                &|value, layout| self.patch_base_value(value, layout),
            )?;

        if matches!(group_read, GroupReadResult::Uninitialized) {
            self.initialize_mvhashmap_base_group_contents(group_key)?;

            group_read = self
                .latest_view
                .get_resource_group_state()
                .read_cached_group_tagged_data(
                    self.txn_idx,
                    group_key,
                    resource_tag,
                    maybe_layout,
                    &|value, layout| self.patch_base_value(value, layout),
                )?;
        };

        Ok(group_read.into_value().0)
    }

    fn resource_size_in_group(
        &self,
        _group_key: &Self::GroupKey,
        _resource_tag: &Self::ResourceTag,
    ) -> anyhow::Result<usize> {
        unimplemented!("Currently resolved by ResourceGroupAdapter");
    }

    fn resource_exists_in_group(
        &self,
        _group_key: &Self::GroupKey,
        _resource_tag: &Self::ResourceTag,
    ) -> anyhow::Result<bool> {
        unimplemented!("Currently resolved by ResourceGroupAdapter");
    }

    fn release_group_cache(
        &self,
    ) -> Option<HashMap<Self::GroupKey, BTreeMap<Self::ResourceTag, Bytes>>> {
        unimplemented!("Currently resolved by ResourceGroupAdapter");
    }

    fn is_resource_group_split_in_change_set_capable(&self) -> bool {
        match &self.latest_view {
            ViewState::Sync(_) => true,
            ViewState::Unsync(state) => state.dynamic_change_set_optimizations_enabled,
        }
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> TModuleView
    for LatestView<'a, T, S, X>
{
    type Key = T::Key;

    fn get_module_state_value(&self, state_key: &Self::Key) -> anyhow::Result<Option<StateValue>> {
        debug_assert!(
            state_key.module_path().is_some(),
            "Reading a resource {:?} using ModuleView",
            state_key,
        );

        match &self.latest_view {
            ViewState::Sync(state) => {
                use MVModulesError::*;
                use MVModulesOutput::*;

                match state.fetch_module(state_key, self.txn_idx) {
                    Ok(Executable(_)) => unreachable!("Versioned executable not implemented"),
                    Ok(Module((v, _))) => Ok(v.as_state_value()),
                    Err(Dependency(_)) => {
                        // Return anything (e.g. module does not exist) to avoid waiting,
                        // because parallel execution will fall back to sequential anyway.
                        Ok(None)
                    },
                    Err(NotFound) => self.get_raw_base_value(state_key),
                }
            },
            ViewState::Unsync(state) => state.unsync_map.fetch_module_data(state_key).map_or_else(
                || self.get_raw_base_value(state_key),
                |v| Ok(v.as_state_value()),
            ),
        }
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> StateStorageView
    for LatestView<'a, T, S, X>
{
    fn id(&self) -> StateViewId {
        self.base_view.id()
    }

    fn get_usage(&self) -> anyhow::Result<StateStorageUsage> {
        self.base_view.get_usage()
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> TAggregatorV1View
    for LatestView<'a, T, S, X>
{
    type Identifier = T::Key;

    fn get_aggregator_v1_state_value(
        &self,
        state_key: &Self::Identifier,
    ) -> anyhow::Result<Option<StateValue>> {
        // TODO[agg_v1](cleanup):
        // Integrate aggregators V1. That is, we can lift the u128 value
        // from the state item by passing the right layout here. This can
        // be useful for cross-testing the old and the new flows.
        // self.get_resource_state_value(state_key, Some(&MoveTypeLayout::U128))
        self.get_resource_state_value(state_key, None)
    }
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> TDelayedFieldView
    for LatestView<'a, T, S, X>
{
    type Identifier = T::Identifier;
    type ResourceGroupTag = T::Tag;
    type ResourceKey = T::Key;
    type ResourceValue = T::Value;

    fn is_delayed_field_optimization_capable(&self) -> bool {
        match &self.latest_view {
            ViewState::Sync(_) => true,
            ViewState::Unsync(state) => state.dynamic_change_set_optimizations_enabled,
        }
    }

    fn get_delayed_field_value(
        &self,
        id: &Self::Identifier,
    ) -> Result<DelayedFieldValue, PanicOr<DelayedFieldsSpeculativeError>> {
        match &self.latest_view {
            ViewState::Sync(state) => get_delayed_field_value_impl(&state.captured_reads, state.versioned_map.delayed_fields(), state.scheduler, id, self.txn_idx),
            ViewState::Unsync(state) => Ok(state.unsync_map.fetch_delayed_field(id).ok_or_else(|| {
                code_invariant_error(format!("DelayedField {:?} not found in get_delayed_field_value in sequential execution", id))
            })?),
        }
    }

    fn delayed_field_try_add_delta_outcome(
        &self,
        id: &Self::Identifier,
        base_delta: &SignedU128,
        delta: &SignedU128,
        max_value: u128,
    ) -> Result<bool, PanicOr<DelayedFieldsSpeculativeError>> {
        match &self.latest_view {
            ViewState::Sync(state) => delayed_field_try_add_delta_outcome_impl(
                &state.captured_reads,
                state.versioned_map.delayed_fields(),
                state.scheduler,
                id,
                base_delta,
                delta,
                max_value,
                self.txn_idx,
            ),
            ViewState::Unsync(state) => {
                // No speculation in sequential execution, just evaluate directly
                let value = state.unsync_map
                    .fetch_delayed_field(id)
                    .ok_or_else(|| {
                        code_invariant_error(format!("DelayedField {:?} not found in delayed_field_try_add_delta_outcome in sequential execution", id))
                    })?
                    .into_aggregator_value()?;
                let math = BoundedMath::new(max_value);
                let before = expect_ok(math.unsigned_add_delta(value, base_delta))?;
                if math.unsigned_add_delta(before, delta).is_err() {
                    Ok(false)
                } else {
                    Ok(true)
                }
            },
        }
    }

    fn generate_delayed_field_id(&self) -> Self::Identifier {
        match &self.latest_view {
            ViewState::Sync(state) => (state.counter.fetch_add(1, Ordering::SeqCst) as u64).into(),
            ViewState::Unsync(state) => {
                let mut counter = state.counter.borrow_mut();
                let id = (*counter as u64).into();
                *counter += 1;
                id
            },
        }
    }

    fn validate_and_convert_delayed_field_id(
        &self,
        id: u64,
    ) -> Result<Self::Identifier, PanicError> {
        let start_counter = match &self.latest_view {
            ViewState::Sync(state) => state.start_counter,
            ViewState::Unsync(state) => state.start_counter,
        };

        if id < start_counter as u64 {
            return Err(code_invariant_error(format!(
                "Invalid delayed field id: {}, we've started from {}",
                id, start_counter
            )));
        }

        let current = match &self.latest_view {
            ViewState::Sync(state) => state.counter.load(Ordering::SeqCst),
            ViewState::Unsync(state) => *state.counter.borrow(),
        };

        if id > current as u64 {
            return Err(code_invariant_error(format!(
                "Invalid delayed field id: {}, we've only reached to {}",
                id, current
            )));
        }

        Ok(id.into())
    }

    // TODO - update comment.
    // For each resource that satisfies the following conditions,
    //     1. Resource is in read set
    //     2. Resource is not in write set
    // replace the delayed field identifiers in the resource with corresponding values.
    // If any of the delayed field identifiers in the resource are part of delayed_field_write_set,
    // then include the resource in the write set.
    fn get_reads_needing_exchange(
        &self,
        delayed_write_set_keys: &HashSet<Self::Identifier>,
        skip: &HashSet<Self::ResourceKey>,
    ) -> Result<BTreeMap<Self::ResourceKey, (Self::ResourceValue, Arc<MoveTypeLayout>)>, PanicError>
    {
        match &self.latest_view {
            ViewState::Sync(state) => {
                let captured_reads = state.captured_reads.borrow();
                self.get_reads_needing_exchange_parallel(
                    &captured_reads,
                    delayed_write_set_keys,
                    skip,
                )
            },
            ViewState::Unsync(state) => {
                let resource_with_layout_read_set = state.resource_with_layout_read_set.borrow();
                self.get_reads_needing_exchange_sequential(
                    &resource_with_layout_read_set,
                    state.unsync_map,
                    delayed_write_set_keys,
                    skip,
                )
            },
        }
    }

    fn get_group_reads_needing_exchange(
        &self,
        delayed_write_set_keys: &HashSet<Self::Identifier>,
        skip: &HashSet<Self::ResourceKey>,
    ) -> Result<BTreeMap<Self::ResourceKey, (Self::ResourceValue, u64)>, PanicError> {
        match &self.latest_view {
            ViewState::Sync(state) => {
                self.get_group_reads_needing_exchange_parallel(state, delayed_write_set_keys, skip)
            },
            ViewState::Unsync(state) => {
                let group_read_set = state.group_read_set.borrow();
                self.get_group_reads_needing_exchange_sequential(
                    &group_read_set,
                    state.unsync_map,
                    delayed_write_set_keys,
                    skip,
                )
            },
        }
    }
}

struct TemporaryValueToIdentifierMapping<
    'a,
    T: Transaction,
    S: TStateView<Key = T::Key>,
    X: Executable,
> {
    latest_view: &'a LatestView<'a, T, S, X>,
    txn_idx: TxnIndex,
    // These are the delayed field keys that were touched when utilizing this mapping
    // to replace ids with values or values with ids
    delayed_field_keys: RefCell<HashSet<T::Identifier>>,
}

impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable>
    TemporaryValueToIdentifierMapping<'a, T, S, X>
{
    pub fn new(latest_view: &'a LatestView<'a, T, S, X>, txn_idx: TxnIndex) -> Self {
        Self {
            latest_view,
            txn_idx,
            delayed_field_keys: RefCell::new(HashSet::new()),
        }
    }

    fn generate_delayed_field_id(&self) -> T::Identifier {
        self.latest_view.generate_delayed_field_id()
    }

    pub fn into_inner(self) -> HashSet<T::Identifier> {
        self.delayed_field_keys.into_inner()
    }
}

// For aggregators V2, values are replaced with identifiers at deserialization time,
// and are replaced back when the value is serialized. The "lifted" values are cached
// by the `LatestView` in the aggregators multi-version data structure.
impl<'a, T: Transaction, S: TStateView<Key = T::Key>, X: Executable> ValueToIdentifierMapping
    for TemporaryValueToIdentifierMapping<'a, T, S, X>
{
    fn value_to_identifier(
        &self,
        kind: &IdentifierMappingKind,
        layout: &MoveTypeLayout,
        value: Value,
    ) -> TransformationResult<Value> {
        let id = self.generate_delayed_field_id();
        let base_value = DelayedFieldValue::try_from_move_value(layout, value, kind)?;
        match &self.latest_view.latest_view {
            ViewState::Sync(state) => state.set_delayed_field_value(id, base_value),
            ViewState::Unsync(state) => {
                state.set_delayed_field_value(id, base_value);
            },
        };
        self.delayed_field_keys.borrow_mut().insert(id);
        id.try_into_move_value(layout)
            .map_err(|e| TransformationError(format!("{:?}", e)))
    }

    fn identifier_to_value(
        &self,
        layout: &MoveTypeLayout,
        identifier_value: Value,
    ) -> TransformationResult<Value> {
        let id = T::Identifier::try_from_move_value(layout, identifier_value, &())
            .map_err(|e| TransformationError(format!("{:?}", e)))?;
        self.delayed_field_keys.borrow_mut().insert(id);
        match &self.latest_view.latest_view {
            ViewState::Sync(state) => Ok(state
                .versioned_map
                .delayed_fields()
                .read_latest_committed_value(&id, self.txn_idx, ReadPosition::AfterCurrentTxn)
                .expect("Committed value for ID must always exist")
                .try_into_move_value(layout)?),
            ViewState::Unsync(state) => Ok(state
                .read_delayed_field(id)
                .expect("Delayed field value for ID must always exist in sequential execution")
                .try_into_move_value(layout)?),
        }
    }
}

struct TemporaryExtractIdentifiersMapping<T: Transaction> {
    // These are the delayed field keys that were touched when utilizing this mapping
    // to replace ids with values or values with ids
    delayed_field_keys: RefCell<HashSet<T::Identifier>>,
}

impl<T: Transaction> TemporaryExtractIdentifiersMapping<T> {
    pub fn new() -> Self {
        Self {
            delayed_field_keys: RefCell::new(HashSet::new()),
        }
    }

    pub fn into_inner(self) -> HashSet<T::Identifier> {
        self.delayed_field_keys.into_inner()
    }
}

impl<T: Transaction> ValueToIdentifierMapping for TemporaryExtractIdentifiersMapping<T> {
    fn value_to_identifier(
        &self,
        _kind: &IdentifierMappingKind,
        layout: &MoveTypeLayout,
        value: Value,
    ) -> TransformationResult<Value> {
        let id = T::Identifier::try_from_move_value(layout, value, &())
            .map_err(|e| TransformationError(format!("{:?}", e)))?;
        self.delayed_field_keys.borrow_mut().insert(id);
        id.try_into_move_value(layout)
            .map_err(|e| TransformationError(format!("{:?}", e)))
    }

    fn identifier_to_value(
        &self,
        layout: &MoveTypeLayout,
        identifier_value: Value,
    ) -> TransformationResult<Value> {
        let id = T::Identifier::try_from_move_value(layout, identifier_value, &())
            .map_err(|e| TransformationError(format!("{:?}", e)))?;
        self.delayed_field_keys.borrow_mut().insert(id);
        id.try_into_move_value(layout)
            .map_err(|e| TransformationError(format!("{:?}", e)))
    }
}

#[cfg(test)]
mod test {
    //
    // TODO[agg_v2](tests): Refactor the tests, and include resource groups in testing
    //
    use super::*;
    use crate::{
        captured_reads::{CapturedReads, DelayedFieldRead, DelayedFieldReadKind},
        proptest_types::types::{KeyType, MockEvent, ValueType},
        scheduler::{DependencyResult, Scheduler, TWaitForDependency},
        view::{delayed_field_try_add_delta_outcome_impl, get_delayed_field_value_impl, ViewState},
    };
    use aptos_aggregator::{
        bounded_math::{BoundedMath, SignedU128},
        delta_math::DeltaHistory,
        types::{DelayedFieldValue, DelayedFieldsSpeculativeError, PanicOr, ReadPosition},
    };
    use aptos_mvhashmap::{
        types::{MVDelayedFieldsError, TxnIndex},
        unsync_map::UnsyncMap,
        versioned_delayed_fields::TVersionedDelayedFieldView,
        MVHashMap,
    };
    use aptos_state_view::TStateView;
    use aptos_types::{
        aggregator::DelayedFieldID,
        executable::Executable,
        state_store::{state_storage_usage::StateStorageUsage, state_value::StateValue},
        transaction::BlockExecutableTransaction,
        write_set::TransactionWrite,
    };
    use aptos_vm_types::resolver::TResourceView;
    use bytes::Bytes;
    use claims::{assert_err_eq, assert_none, assert_ok_eq, assert_some_eq};
    use move_core_types::value::{
        IdentifierMappingKind, LayoutTag, MoveStructLayout, MoveTypeLayout,
    };
    use move_vm_types::values::{Struct, Value};
    use std::{cell::RefCell, collections::HashMap, sync::atomic::AtomicU32};
    use test_case::test_case;

    #[derive(Default)]
    pub struct FakeVersionedDelayedFieldView {
        data: HashMap<DelayedFieldID, DelayedFieldValue>,
    }

    impl FakeVersionedDelayedFieldView {
        pub fn set_value(&mut self, id: DelayedFieldID, value: DelayedFieldValue) {
            self.data.insert(id, value);
        }
    }

    impl TVersionedDelayedFieldView<DelayedFieldID> for FakeVersionedDelayedFieldView {
        fn read(
            &self,
            id: &DelayedFieldID,
            _txn_idx: TxnIndex,
        ) -> Result<DelayedFieldValue, PanicOr<MVDelayedFieldsError>> {
            self.data
                .get(id)
                .cloned()
                .ok_or(PanicOr::Or(MVDelayedFieldsError::NotFound))
        }

        fn read_latest_committed_value(
            &self,
            id: &DelayedFieldID,
            _current_txn_idx: TxnIndex,
            _read_position: ReadPosition,
        ) -> Result<DelayedFieldValue, MVDelayedFieldsError> {
            self.data
                .get(id)
                .cloned()
                .ok_or(MVDelayedFieldsError::NotFound)
        }
    }

    struct FakeWaitForDependency();

    impl TWaitForDependency for FakeWaitForDependency {
        fn wait_for_dependency(
            &self,
            _txn_idx: TxnIndex,
            _dep_txn_idx: TxnIndex,
        ) -> DependencyResult {
            unreachable!();
        }
    }

    #[derive(Clone, Debug)]
    struct TestTransactionType {}

    impl BlockExecutableTransaction for TestTransactionType {
        type Event = MockEvent;
        type Identifier = DelayedFieldID;
        type Key = KeyType<u32>;
        type Tag = u32;
        type Value = ValueType;
    }

    #[test]
    fn test_history_updates() {
        let mut view = FakeVersionedDelayedFieldView::default();
        let captured_reads = RefCell::new(CapturedReads::<TestTransactionType>::new());
        let wait_for = FakeWaitForDependency();
        let id = DelayedFieldID::new(600);
        let max_value = 600;
        let math = BoundedMath::new(max_value);
        let txn_idx = 1;
        let storage_value = 100;
        view.set_value(id, DelayedFieldValue::Aggregator(storage_value));

        let mut base_delta = SignedU128::Positive(0);
        let base_value_ref = &mut base_delta;

        macro_rules! assert_try_add {
            ($delta:expr, $outcome:expr) => {
                assert_ok_eq!(
                    delayed_field_try_add_delta_outcome_impl(
                        &captured_reads,
                        &view,
                        &wait_for,
                        &id,
                        base_value_ref,
                        &$delta,
                        max_value,
                        txn_idx
                    ),
                    $outcome
                );
                if $outcome {
                    *base_value_ref = math.signed_add(base_value_ref, &$delta).unwrap();
                }
            };
        }

        assert_try_add!(SignedU128::Positive(300), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(100), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(450), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(200), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(350), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 500,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(600), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 500,
                    min_achieved_negative_delta: 100,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );
    }

    #[test]
    fn test_aggregator_overflows() {
        let mut view = FakeVersionedDelayedFieldView::default();
        let captured_reads = RefCell::new(CapturedReads::<TestTransactionType>::new());
        let wait_for = FakeWaitForDependency();
        let id = DelayedFieldID::new(600);
        let max_value = 600;
        let math = BoundedMath::new(max_value);
        let txn_idx = 1;
        let storage_value = 100;
        view.set_value(id, DelayedFieldValue::Aggregator(storage_value));

        let mut base_delta = SignedU128::Positive(0);
        let base_value_ref = &mut base_delta;

        macro_rules! assert_try_add {
            ($delta:expr, $outcome:expr) => {
                assert_ok_eq!(
                    delayed_field_try_add_delta_outcome_impl(
                        &captured_reads,
                        &view,
                        &wait_for,
                        &id,
                        base_value_ref,
                        &$delta,
                        max_value,
                        txn_idx
                    ),
                    $outcome
                );
                if $outcome {
                    *base_value_ref = math.signed_add(base_value_ref, &$delta).unwrap();
                }
            };
        }

        assert_try_add!(SignedU128::Positive(400), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(450), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(601), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(575), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: Some(525),
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(551), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: Some(501),
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Positive(570), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 400,
                    min_achieved_negative_delta: 50,
                    min_overflow_positive_delta: Some(501),
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );
    }

    #[test]
    fn test_aggregator_underflows() {
        let mut view = FakeVersionedDelayedFieldView::default();
        let captured_reads = RefCell::new(CapturedReads::<TestTransactionType>::new());
        let wait_for = FakeWaitForDependency();
        let id = DelayedFieldID::new(600);
        let max_value = 600;
        let math = BoundedMath::new(max_value);
        let txn_idx = 1;
        let storage_value = 200;
        view.set_value(id, DelayedFieldValue::Aggregator(storage_value));

        let mut base_delta = SignedU128::Positive(0);
        let base_value_ref = &mut base_delta;

        macro_rules! assert_try_add {
            ($delta:expr, $outcome:expr) => {
                assert_ok_eq!(
                    delayed_field_try_add_delta_outcome_impl(
                        &captured_reads,
                        &view,
                        &wait_for,
                        &id,
                        base_value_ref,
                        &$delta,
                        max_value,
                        txn_idx
                    ),
                    $outcome
                );
                if $outcome {
                    *base_value_ref = math.signed_add(base_value_ref, &$delta).unwrap();
                }
            };
        }

        assert_try_add!(SignedU128::Positive(300), true);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(650), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(550), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: Some(250),
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(525), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: Some(225),
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(540), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: Some(225),
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        assert_try_add!(SignedU128::Negative(501), false);
        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: Some(201),
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );
    }

    #[test]
    fn test_read_kind_upgrade_fail() {
        let mut view = FakeVersionedDelayedFieldView::default();
        let captured_reads = RefCell::new(CapturedReads::<TestTransactionType>::new());
        let wait_for = FakeWaitForDependency();
        let id = DelayedFieldID::new(600);
        let max_value = 600;
        let txn_idx = 1;
        let storage_value = 200;
        view.set_value(id, DelayedFieldValue::Aggregator(storage_value));

        assert_ok_eq!(
            delayed_field_try_add_delta_outcome_impl(
                &captured_reads,
                &view,
                &wait_for,
                &id,
                &SignedU128::Positive(0),
                &SignedU128::Positive(300),
                max_value,
                txn_idx
            ),
            true
        );

        assert_some_eq!(
            captured_reads
                .borrow()
                .get_delayed_field_by_kind(&id, DelayedFieldReadKind::HistoryBounded),
            DelayedFieldRead::HistoryBounded {
                restriction: DeltaHistory {
                    max_achieved_positive_delta: 300,
                    min_achieved_negative_delta: 0,
                    min_overflow_positive_delta: None,
                    max_underflow_negative_delta: None,
                },
                max_value,
                inner_aggregator_value: storage_value,
            }
        );

        view.set_value(id, DelayedFieldValue::Aggregator(400));
        assert_err_eq!(
            get_delayed_field_value_impl(&captured_reads, &view, &wait_for, &id, txn_idx),
            PanicOr::Or(DelayedFieldsSpeculativeError::InconsistentRead),
        );
    }

    // TODO: Check how to import MockStateView from other tests
    // rather than rewriting it here again
    struct MockStateView {
        data: HashMap<KeyType<u32>, StateValue>,
    }

    impl MockStateView {
        fn new(data: HashMap<KeyType<u32>, StateValue>) -> Self {
            Self { data }
        }
    }

    impl TStateView for MockStateView {
        type Key = KeyType<u32>;

        fn get_state_value(&self, state_key: &Self::Key) -> anyhow::Result<Option<StateValue>> {
            Ok(self.data.get(state_key).cloned())
        }

        fn get_usage(&self) -> anyhow::Result<StateStorageUsage> {
            unimplemented!();
        }
    }

    #[derive(Clone)]
    struct MockExecutable {}

    impl Executable for MockExecutable {
        fn size_bytes(&self) -> usize {
            unimplemented!();
        }
    }

    #[test]
    fn test_id_value_exchange() {
        // Test that replace_values_with_identifiers and replace_identifiers_with_values functions are working correctly

        // Create latest_view
        let unsync_map = UnsyncMap::new();
        let counter = RefCell::new(5);
        let base_view = MockStateView::new(HashMap::new());
        let latest_view = LatestView::<TestTransactionType, MockStateView, MockExecutable>::new(
            &base_view,
            ViewState::Unsync(SequentialState::new(&unsync_map, 5, &counter, true)),
            1,
        );

        // Test id -- value exchange for a value that does not contain delayed fields
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![
            MoveTypeLayout::U64,
            MoveTypeLayout::U64,
            MoveTypeLayout::U64,
        ]));
        let value = Value::struct_(Struct::pack(vec![
            Value::u64(1),
            Value::u64(2),
            Value::u64(3),
        ]));
        let state_value = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        let (patched_state_value, identifiers) = latest_view
            .replace_values_with_identifiers(state_value.clone(), &layout)
            .unwrap();
        assert_eq!(state_value, patched_state_value);
        assert!(
            identifiers.is_empty(),
            "No identifiers should have been replaced in this case"
        );
        let (final_state_value, identifiers) = latest_view
            .replace_identifiers_with_values(patched_state_value.bytes(), &layout)
            .unwrap();
        assert_eq!(state_value, StateValue::from(final_state_value.to_vec()));
        assert!(
            identifiers.is_empty(),
            "No identifiers should have been replaced in this case"
        );

        /*
            layout = Struct {
                agg: Aggregator<u64>
            }
        */
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Struct(
            MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Aggregator),
                    Box::new(MoveTypeLayout::U64),
                ),
                MoveTypeLayout::U64,
            ]),
        )]));
        let value = Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
            Value::u64(25),
            Value::u64(30),
        ]))]));
        let state_value = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        let (patched_state_value, identifiers) = latest_view
            .replace_values_with_identifiers(state_value.clone(), &layout)
            .unwrap();
        assert!(
            identifiers.len() == 1,
            "One identifier should have been replaced in this case"
        );
        assert!(
            identifiers.contains(&DelayedFieldID::new(5)),
            "The value 25 should have been replaced in the identifier 5"
        );
        let (final_state_value, identifiers) = latest_view
            .replace_identifiers_with_values(patched_state_value.bytes(), &layout)
            .unwrap();
        assert_eq!(state_value, StateValue::from(final_state_value.to_vec()));
        assert!(
            identifiers.len() == 1,
            "One identifier should have been replaced in this case"
        );

        /*
            layout = Struct {
                aggregators: vec![Aggregator<u64>]
            }
        */
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Vector(
            Box::new(MoveTypeLayout::Struct(MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Aggregator),
                    Box::new(MoveTypeLayout::U64),
                ),
                MoveTypeLayout::U64,
            ]))),
        )]));
        let value = Value::struct_(Struct::pack(vec![Value::vector_for_testing_only(vec![
            Value::struct_(Struct::pack(vec![Value::u64(20), Value::u64(50)])),
            Value::struct_(Struct::pack(vec![Value::u64(35), Value::u64(65)])),
            Value::struct_(Struct::pack(vec![Value::u64(0), Value::u64(20)])),
        ])]));
        let state_value = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        let (patched_state_value, identifiers) = latest_view
            .replace_values_with_identifiers(state_value.clone(), &layout)
            .unwrap();
        assert!(
            identifiers.len() == 3,
            "Three identifiers should have been replaced in this case"
        );
        assert!(
            counter == RefCell::new(9),
            "The counter should have been updated to 9"
        );
        let patched_value =
            Value::struct_(Struct::pack(vec![Value::vector_for_testing_only(vec![
                Value::struct_(Struct::pack(vec![Value::u64(6), Value::u64(50)])),
                Value::struct_(Struct::pack(vec![Value::u64(7), Value::u64(65)])),
                Value::struct_(Struct::pack(vec![Value::u64(8), Value::u64(20)])),
            ])]));
        assert_eq!(
            patched_state_value,
            StateValue::new_legacy(patched_value.simple_serialize(&layout).unwrap().into())
        );
        let (final_state_value, identifiers) = latest_view
            .replace_identifiers_with_values(patched_state_value.bytes(), &layout)
            .unwrap();
        assert_eq!(state_value, StateValue::from(final_state_value.to_vec()));
        assert!(
            identifiers.len() == 3,
            "Three identifiers should have been replaced in this case"
        );

        /*
            layout = Struct {
                aggregators: vec![AggregatorSnapshot<u128>]
            }
        */
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Vector(
            Box::new(MoveTypeLayout::Struct(MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Snapshot),
                    Box::new(MoveTypeLayout::U128),
                ),
            ]))),
        )]));
        let value = Value::struct_(Struct::pack(vec![Value::vector_for_testing_only(vec![
            Value::struct_(Struct::pack(vec![Value::u128(20)])),
            Value::struct_(Struct::pack(vec![Value::u128(35)])),
            Value::struct_(Struct::pack(vec![Value::u128(0)])),
        ])]));
        let state_value = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        let (patched_state_value, identifiers) = latest_view
            .replace_values_with_identifiers(state_value.clone(), &layout)
            .unwrap();
        assert!(
            identifiers.len() == 3,
            "Three identifiers should have been replaced in this case"
        );
        assert!(
            counter == RefCell::new(12),
            "The counter should have been updated to 12"
        );
        let patched_value =
            Value::struct_(Struct::pack(vec![Value::vector_for_testing_only(vec![
                Value::struct_(Struct::pack(vec![Value::u128(9)])),
                Value::struct_(Struct::pack(vec![Value::u128(10)])),
                Value::struct_(Struct::pack(vec![Value::u128(11)])),
            ])]));
        assert_eq!(
            patched_state_value,
            StateValue::new_legacy(patched_value.simple_serialize(&layout).unwrap().into())
        );
        let (final_state_value, identifiers2) = latest_view
            .replace_identifiers_with_values(patched_state_value.bytes(), &layout)
            .unwrap();
        assert_eq!(state_value, StateValue::from(final_state_value.to_vec()));
        assert!(
            identifiers2.len() == 3,
            "Three identifiers should have been replaced in this case"
        );
        assert_eq!(identifiers, identifiers2);

        /*
            layout = Struct {
                snap: vec![AggregatorSnapshot<string>]
            }
        */
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Vector(
            Box::new(MoveTypeLayout::Struct(MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Snapshot),
                    Box::new(MoveTypeLayout::Struct(MoveStructLayout::Runtime(vec![
                        MoveTypeLayout::Vector(Box::new(MoveTypeLayout::U8)),
                    ]))),
                ),
            ]))),
        )]));
        let value = Value::struct_(Struct::pack(vec![Value::vector_for_testing_only(vec![
            Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
                Value::vector_u8(bcs::to_bytes("hello").unwrap().to_vec()),
            ]))])),
            Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
                Value::vector_u8(bcs::to_bytes("ab").unwrap().to_vec()),
            ]))])),
            Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
                Value::vector_u8(bcs::to_bytes("c").unwrap().to_vec()),
            ]))])),
        ])]));
        let state_value = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        let (patched_state_value, identifiers) = latest_view
            .replace_values_with_identifiers(state_value.clone(), &layout)
            .unwrap();
        assert!(
            identifiers.len() == 3,
            "Three identifiers should have been replaced in this case"
        );
        assert!(
            counter == RefCell::new(15),
            "The counter should have been updated to 15"
        );
        // TODO: This assertion is failing. The replaced identifier is not BCS encoded.
        // let patched_value = Value::struct_(Struct::pack(vec![
        //     Value::vector_for_testing_only(vec![
        //         Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![Value::vector_u8(bcs::to_bytes("12").unwrap().to_vec())]))])),
        //         Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![Value::vector_u8(bcs::to_bytes("13").unwrap().to_vec())]))])),
        //         Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![Value::vector_u8(bcs::to_bytes("14").unwrap().to_vec())]))])),
        // ])]));
        // assert_eq!(
        //     patched_state_value,
        //     StateValue::new_legacy(patched_value.simple_serialize(&layout).unwrap().into())
        // );
        let (final_state_value, identifiers2) = latest_view
            .replace_identifiers_with_values(patched_state_value.bytes(), &layout)
            .unwrap();
        assert_eq!(state_value, StateValue::from(final_state_value.to_vec()));
        assert!(
            identifiers2.len() == 3,
            "Three identifiers should have been replaced in this case"
        );
        assert_eq!(identifiers, identifiers2);
    }

    fn create_aggregator_layout() -> MoveTypeLayout {
        MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Struct(
            MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Aggregator),
                    Box::new(MoveTypeLayout::U64),
                ),
                MoveTypeLayout::U64,
            ]),
        )]))
    }

    fn create_aggregator_value(value: u64, max_value: u64) -> Value {
        Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
            Value::u64(value),
            Value::u64(max_value),
        ]))]))
    }

    fn create_state_value(value: &Value, layout: &MoveTypeLayout) -> StateValue {
        StateValue::new_legacy(value.simple_serialize(layout).unwrap().into())
    }

    struct Holder {
        unsync_map: UnsyncMap<KeyType<u32>, u32, ValueType, MockExecutable, DelayedFieldID>,
        counter: RefCell<u32>,
        base_view: MockStateView,
    }

    impl Holder {
        fn new(data: HashMap<KeyType<u32>, StateValue>, start_counter: u32) -> Self {
            let unsync_map = UnsyncMap::new();
            let counter = RefCell::new(start_counter);
            let base_view = MockStateView::new(data);
            Self {
                unsync_map,
                counter,
                base_view,
            }
        }
    }

    fn create_sequential_latest_view<'a>(
        h: &'a Holder,
        dynamic_change_set_optimizations_enabled: bool,
    ) -> LatestView<'a, TestTransactionType, MockStateView, MockExecutable> {
        let sequential_state: SequentialState<'a, TestTransactionType, MockExecutable> =
            SequentialState::new(
                &h.unsync_map,
                *h.counter.borrow(),
                &h.counter,
                dynamic_change_set_optimizations_enabled,
            );

        LatestView::<'a, TestTransactionType, MockStateView, MockExecutable>::new(
            &h.base_view,
            ViewState::Unsync(sequential_state),
            1,
        )
    }

    #[test]
    fn test_sequential_missing_not_recorded() {
        let h = Holder::new(HashMap::new(), 1000);
        let latest_view = create_sequential_latest_view(&h, true);

        assert_ok_eq!(
            latest_view.get_resource_state_value(&KeyType::<u32>(1, false), None),
            None
        );
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());

        assert_ok_eq!(
            latest_view.resource_exists(&KeyType::<u32>(1, false)),
            false,
        );
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());

        assert_ok_eq!(
            latest_view.get_resource_state_value_metadata(&KeyType::<u32>(1, false)),
            None,
        );
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());
    }

    #[test]
    fn test_sequential_non_value_reads_not_recorded() {
        let state_value = create_state_value(&Value::u64(12321), &MoveTypeLayout::U64);
        let data = HashMap::from([(KeyType::<u32>(1, false), state_value.clone())]);
        let h = Holder::new(data, 1000);
        let latest_view = create_sequential_latest_view(&h, true);

        assert_ok_eq!(latest_view.resource_exists(&KeyType::<u32>(1, false)), true,);
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());

        assert!(latest_view
            .get_resource_state_value_metadata(&KeyType::<u32>(1, false))
            .unwrap()
            .is_some(),);
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());
    }

    fn assert_fetch_eq<V: TransactionWrite>(
        fetched: Option<ValueWithLayout<V>>,
        expected_maybe_write: Option<V>,
        expected_maybe_layout: Option<MoveTypeLayout>,
    ) {
        match fetched {
            Some(ValueWithLayout::Exchanged(write, layout)) => {
                let expected_write = expected_maybe_write.unwrap();
                assert_eq!(write.write_op_kind(), expected_write.write_op_kind());
                assert_eq!(write.bytes(), expected_write.bytes());
                assert_eq!(write.as_state_value(), expected_write.as_state_value());
                assert_eq!(
                    layout.as_ref().map(|v| v.as_ref()),
                    expected_maybe_layout.as_ref()
                );
            },
            Some(ValueWithLayout::RawFromStorage(_)) => panic!("Unexpected RawFromStorage"),
            None => {
                assert_none!(expected_maybe_write);
                assert_none!(expected_maybe_layout);
            },
        }
    }

    #[test]
    fn test_sequential_regular_read_operations() {
        let state_value = create_state_value(&Value::u64(12321), &MoveTypeLayout::U64);
        let data = HashMap::from([(KeyType::<u32>(1, false), state_value.clone())]);
        let h = Holder::new(data, 1000);
        let latest_view = create_sequential_latest_view(&h, true);

        assert_ok_eq!(
            latest_view.get_resource_state_value(&KeyType::<u32>(1, false), None),
            Some(state_value.clone())
        );
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .is_empty());

        assert_fetch_eq(
            h.unsync_map.fetch_data(&KeyType::<u32>(1, false)),
            Some(TransactionWrite::from_state_value(Some(state_value))),
            None,
        );
    }

    #[test_case(Some(true))]
    #[test_case(Some(false))]
    #[test_case(None)]
    fn test_sequential_aggregator_read_operations(check_metadata: Option<bool>) {
        let layout = create_aggregator_layout();
        let value = create_aggregator_value(25, 30);
        let state_value = create_state_value(&value, &layout);
        let data = HashMap::from([(KeyType::<u32>(1, false), state_value.clone())]);
        let h = Holder::new(data, 1000);
        let latest_view = create_sequential_latest_view(&h, true);

        let patched_value = create_aggregator_value(1000, 30);
        let patched_state_value = create_state_value(&patched_value, &layout);

        match check_metadata {
            Some(true) => {
                latest_view
                    .get_resource_state_value_metadata(&KeyType::<u32>(1, false))
                    .unwrap();
            },
            Some(false) => {
                assert_ok_eq!(latest_view.resource_exists(&KeyType::<u32>(1, false)), true,);
            },
            None => {},
        };

        assert_ok_eq!(
            latest_view.get_resource_state_value(&KeyType::<u32>(1, false), Some(&layout)),
            Some(patched_state_value.clone())
        );
        assert!(latest_view
            .get_resource_with_layout_read_set_sequential()
            .contains(&KeyType(1, false)));
        assert_fetch_eq(
            h.unsync_map.fetch_data(&KeyType::<u32>(1, false)),
            Some(TransactionWrite::from_state_value(Some(
                patched_state_value,
            ))),
            Some(layout),
        );
    }

    #[test]
    fn test_read_operations_parallel() {
        let counter = AtomicU32::new(5);
        let state_value_3 = StateValue::new_legacy(Bytes::from(
            Value::u64(12321)
                .simple_serialize(&MoveTypeLayout::U64)
                .unwrap(),
        ));
        let mut data = HashMap::new();
        data.insert(KeyType::<u32>(3, false), state_value_3.clone());
        let layout = MoveTypeLayout::Struct(MoveStructLayout::new(vec![MoveTypeLayout::Struct(
            MoveStructLayout::new(vec![
                MoveTypeLayout::Tagged(
                    LayoutTag::IdentifierMapping(IdentifierMappingKind::Aggregator),
                    Box::new(MoveTypeLayout::U64),
                ),
                MoveTypeLayout::U64,
            ]),
        )]));
        let value = Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
            Value::u64(25),
            Value::u64(30),
        ]))]));
        let state_value_4 = StateValue::new_legacy(value.simple_serialize(&layout).unwrap().into());
        data.insert(KeyType::<u32>(4, false), state_value_4);
        let base_view = MockStateView::new(data);
        let versioned_map = MVHashMap::new();
        let scheduler = Scheduler::new(10);
        let latest_view = LatestView::<TestTransactionType, MockStateView, MockExecutable>::new(
            &base_view,
            ViewState::Sync(ParallelState::new(&versioned_map, &scheduler, 5, &counter)),
            1,
        );

        assert_eq!(
            latest_view
                .get_resource_state_value(&KeyType::<u32>(1, false), None)
                .unwrap(),
            None
        );
        assert_eq!(
            latest_view
                .get_resource_state_value(&KeyType::<u32>(2, false), Some(&layout))
                .unwrap(),
            None
        );
        assert_eq!(
            latest_view
                .get_resource_state_value(&KeyType::<u32>(3, false), None)
                .unwrap(),
            Some(state_value_3.clone())
        );

        //TODO: This is printing Ok(Versioned(Err(StorageVersion), ValueType { bytes: Some(b"!0\0\0\0\0\0\0"), metadata: None }, None))
        // Is Err(StorageVersion) expected here?
        println!(
            "data: {:?}",
            versioned_map
                .data()
                .fetch_data(&KeyType::<u32>(3, false), 1)
        );

        let patched_value = Value::struct_(Struct::pack(vec![Value::struct_(Struct::pack(vec![
            Value::u64(5),
            Value::u64(30),
        ]))]));
        let state_value_4 =
            StateValue::new_legacy(patched_value.simple_serialize(&layout).unwrap().into());
        assert_eq!(
            latest_view
                .get_resource_state_value(&KeyType::<u32>(4, false), Some(&layout))
                .unwrap(),
            Some(state_value_4.clone())
        );

        let captured_reads = latest_view.take_reads();
        assert!(captured_reads.validate_data_reads(versioned_map.data(), 1));
        let read_set_with_delayed_fields = captured_reads.get_read_values_with_delayed_fields();

        // TODO: This prints
        // read: (KeyType(4, false), Versioned(Err(StorageVersion), Some(Struct(Runtime([Struct(Runtime([Tagged(IdentifierMapping(Aggregator), U64), U64]))])))))
        // read: (KeyType(2, false), Versioned(Err(StorageVersion), Some(Struct(Runtime([Struct(Runtime([Tagged(IdentifierMapping(Aggregator), U64), U64]))])))))
        for read in read_set_with_delayed_fields {
            println!("read: {:?}", read);
        }

        // TODO: This assertion fails.
        // let data_read = DataRead::Versioned(Ok((1,0)), Arc::new(TransactionWrite::from_state_value(Some(state_value_4))), Some(Arc::new(layout)));
        // assert!(read_set_with_delayed_fields.any(|x| x == (&KeyType::<u32>(4, false), &data_read)));
    }
}