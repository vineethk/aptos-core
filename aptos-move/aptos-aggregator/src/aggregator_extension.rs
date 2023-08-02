// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    delta_change_set::{addition, subtraction},
    resolver::AggregatorResolver,
};
use aptos_table_natives::TableHandle;
use aptos_types::{state_store::state_key::StateKey, vm_status::StatusCode};
use move_binary_format::errors::{PartialVMError, PartialVMResult};
use move_core_types::account_address::AccountAddress;
use std::collections::{BTreeMap, BTreeSet};

/// Describes the state of each aggregator instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregatorState {
    // If aggregator stores a known value.
    Data,
    // If aggregator stores a non-negative delta.
    PositiveDelta,
    // If aggregator stores a negative delta.
    NegativeDelta,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AggregatorHandle(pub AccountAddress);

/// Uniquely identifies each aggregator instance in storage.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AggregatorID {
    // Aggregator V1 is implemented as a Table item, and so can be queried by the
    // state key.
    Legacy {
        // A handle that is shared across all aggregator instances created by the
        // same `AggregatorFactory` and which is used for fine-grained storage
        // access.
        handle: TableHandle,
        // Unique key associated with each aggregator instance. Generated by
        // taking the hash of transaction which creates an aggregator and the
        // number of aggregators that were created by this transaction so far.
        key: AggregatorHandle,
    },
    // Aggregator V2 is implemented in place with ephemeral identifiers which are
    // unique per block.
    Ephemeral(u64),
}

impl AggregatorID {
    pub fn legacy(handle: TableHandle, key: AggregatorHandle) -> Self {
        AggregatorID::Legacy { handle, key }
    }

    pub fn ephemeral(id: u64) -> Self {
        AggregatorID::Ephemeral(id)
    }

    pub fn as_state_key(&self) -> Option<StateKey> {
        match self {
            AggregatorID::Legacy { handle, key } => {
                Some(StateKey::table_item((*handle).into(), key.0.to_vec()))
            },
            AggregatorID::Ephemeral(_) => None,
        }
    }
}

/// Tracks values seen by aggregator. In particular, stores information about
/// the biggest and the smallest deltas seen during execution in the VM. This
/// information can be used by the executor to check if delta should have
/// failed. Most importantly, it allows commutativity of adds/subs. Example:
///
///
/// This graph shows how delta of aggregator changed during a single transaction
/// execution:
///
/// +A ===========================================>
///            ||
///          ||||                               +X
///         |||||  ||||||                    ||||
///      |||||||||||||||||||||||||          |||||
/// +0 ===========================================> time
///                       ||||||
///                         ||
///                         ||
/// -B ===========================================>
///
/// Clearly, +X succeeds if +A and -B succeed. Therefore each delta
/// validation consists of:
///   1. check +A did not overflow
///   2. check -A did not drop below zero
/// Checking +X is irrelevant since +A >= +X.
///
/// TODO: while we support tracking of the history, it is not yet fully used on
/// executor side because we don't know how to throw errors.
#[derive(Debug)]
pub struct History {
    pub max_achieved_positive: u128,
    pub min_achieved_negative: u128,
    // `min_overflow_positive` is None in two possible cases:
    // 1. No overflow occured in the try_add/try_sub functions throughout the
    // transaction execution.
    // 2. The only overflows that occured in the try_add/try_sub functions in
    // this transaction execution are with delta that exceeds u128::MAX.
    pub min_overflow_positive: Option<u128>,
    // `max_underflow_negative` is None in two possible cases:
    // 1. No underflow occured in the try_add/try_sub functions throughout the
    // transaction execution.
    // 2. The only underflows that occured in the try_add/try_sub functions in
    // this transaction execution are with delta that drops below -u128::MAX.
    pub max_underflow_negative: Option<u128>,
}

impl History {
    fn new() -> Self {
        History {
            max_achieved_positive: 0,
            min_achieved_negative: 0,
            min_overflow_positive: None,
            max_underflow_negative: None,
        }
    }

    fn record_success_positive(&mut self, value: u128) {
        self.max_achieved_positive = u128::max(self.max_achieved_positive, value);
    }

    fn record_success_negative(&mut self, value: u128) {
        self.min_achieved_negative = u128::max(self.min_achieved_negative, value);
    }

    fn record_overflow_positive(&mut self, value: u128) {
        self.min_overflow_positive = match self.min_overflow_positive {
            Some(min) => Some(u128::min(min, value)),
            None => Some(value),
        }
    }

    fn record_underflow_negative(&mut self, value: u128) {
        self.max_underflow_negative = match self.max_underflow_negative {
            Some(min) => Some(u128::min(min, value)),
            None => Some(value),
        }
    }
}

/// Internal aggregator data structure.
#[derive(Debug)]
pub struct Aggregator {
    // Describes a value of an aggregator.
    value: u128,
    // Describes a state of an aggregator.
    state: AggregatorState,
    // Describes an upper bound of an aggregator. If `value` exceeds it, the
    // aggregator overflows.
    // TODO: Currently this is a single u128 value since we use 0 as a trivial
    // lower bound. If we want to support custom lower bounds, or have more
    // complex postconditions, we should factor this out in its own struct.
    limit: u128,
    // Describes values seen by this aggregator. Note that if aggregator knows
    // its value, then storing history doesn't make sense.
    history: Option<History>,
}

impl Aggregator {
    /// Records observed delta in history. Should be called after an operation (addition/subtraction)
    /// is successful to record its side-effects.
    fn record_success(&mut self) {
        if let Some(history) = self.history.as_mut() {
            match self.state {
                AggregatorState::PositiveDelta => history.record_success_positive(self.value),
                AggregatorState::NegativeDelta => history.record_success_negative(self.value),
                AggregatorState::Data => {
                    unreachable!("history is not tracked when aggregator knows its value")
                },
            }
        }
    }

    /// Records overflows in history. Should be called after an addition is unsuccessful
    /// to record its side-effects.
    fn record_overflow(&mut self, value: u128) {
        if let Some(history) = self.history.as_mut() {
            history.record_overflow_positive(value);
        }
    }

    /// Records underflows in history. Should be called after a subtraction is unsuccessful
    /// to record its side-effects.
    fn record_underflow(&mut self, value: u128) {
        if let Some(history) = self.history.as_mut() {
            history.record_underflow_negative(value);
        }
    }

    /// Validates if aggregator's history is correct when applied to
    /// the `base_value`. For example, if history observed a delta of
    /// +100, and the aggregator limit is 150, then the base value of
    /// 60 will not pass validation (60 + 100 > 150), but the base value
    /// of 30 will (30 + 100 < 150).
    fn validate_history(&self, base_value: u128) -> PartialVMResult<()> {
        let history = self
            .history
            .as_ref()
            .expect("History should be set for validation");

        // To validate the history of an aggregator, we want to ensure
        // that there was no violation of postcondition (i.e. overflows or
        // underflows). We can do it by emulating addition and subtraction.
        addition(base_value, history.max_achieved_positive, self.limit)?;
        subtraction(base_value, history.min_achieved_negative)?;
        Ok(())
    }

    /// Implements logic for adding to an aggregator.
    pub fn try_add(&mut self, value: u128) -> PartialVMResult<()> {
        match self.state {
            AggregatorState::Data => {
                // If aggregator knows the value, add directly and keep the state.
                self.value = addition(self.value, value, self.limit)?;
                return Ok(());
            },
            AggregatorState::PositiveDelta => {
                // If positive delta, add directly but also record the state.
                self.value = addition(self.value, value, self.limit).map_err(|err| {
                    if self.value < u128::MAX - value {
                        self.record_overflow(self.value + value);
                    }
                    err
                })?;
            },
            AggregatorState::NegativeDelta => {
                // Negative delta is a special case, since the state might
                // change depending on how big the `value` is. Suppose
                // aggregator has -X and want to do +Y. Then, there are two
                // cases:
                //     1. X <= Y: then the result is +(Y-X)
                //     2. X  > Y: then the result is -(X-Y)
                if self.value <= value {
                    self.value = subtraction(value, self.value)?;
                    self.state = AggregatorState::PositiveDelta;
                    if self.value > self.limit {
                        self.record_overflow(self.value);
                    }
                } else {
                    self.value = subtraction(self.value, value)?;
                }
            },
        }
        self.record_success();
        Ok(())
    }

    /// Implements logic for subtracting from an aggregator.
    pub fn try_sub(&mut self, value: u128) -> PartialVMResult<()> {
        match self.state {
            AggregatorState::Data => {
                // Aggregator knows the value, therefore we can subtract
                // checking we don't drop below zero. We do not need to
                // record the history.
                self.value = subtraction(self.value, value)?;
                return Ok(());
            },
            AggregatorState::PositiveDelta => {
                // Positive delta is a special case because the state can
                // change depending on how big the `value` is. Suppose
                // aggregator has +X and want to do -Y. Then, there are two
                // cases:
                //     1. X >= Y: then the result is +(X-Y)
                //     2. X  < Y: then the result is -(Y-X)
                if self.value >= value {
                    // This case doesn't result in an underflow.
                    // So we are not calling record_underflow here.
                    self.value = subtraction(self.value, value)?;
                } else {
                    // Check that we can subtract in general: we don't want to
                    // allow -10000 when limit is 10.
                    // TODO: maybe `subtraction` should also know about the limit?
                    subtraction(self.limit, value).map_err(|err| {
                        // TODO: The underflow value may not be correct here.
                        self.record_underflow(value);
                        err
                    })?;

                    self.value = subtraction(value, self.value)?;
                    self.state = AggregatorState::NegativeDelta;
                    if value - self.value > self.limit {
                        self.record_underflow(value - self.value);
                    }
                }
            },
            AggregatorState::NegativeDelta => {
                // Since we operate on unsigned integers, we have to add
                // when subtracting from negative delta. Note that if limit
                // is some X, then we cannot subtract more than X, and so
                // we should return an error there.
                self.value = addition(self.value, value, self.limit).map_err(|err| {
                    if self.value < u128::MAX - value {
                        self.record_underflow(self.value + value);
                    }
                    err
                })?;
            },
        }
        self.record_success();
        Ok(())
    }

    /// Implements logic for reading the value of an aggregator. As a
    /// result, the aggregator knows it value (i.e. its state changes to
    /// `Data`).
    pub fn read_and_materialize(
        &mut self,
        resolver: &dyn AggregatorResolver,
        id: &AggregatorID,
    ) -> PartialVMResult<u128> {
        // If aggregator has already been read, return immediately.
        if self.state == AggregatorState::Data {
            return Ok(self.value);
        }

        // Otherwise, we have a delta and have to go to storage and apply it.
        // In theory, any delta will be applied to existing value. However,
        // something may go wrong, so we guard by throwing an error in
        // extension.
        let value_from_storage = resolver.resolve_aggregator_value(id).map_err(|e| {
            extension_error(format!("Could not find the value of the aggregator: {}", e))
        })?;

        // Validate history and apply the delta.
        self.validate_history(value_from_storage)?;
        match self.state {
            AggregatorState::PositiveDelta => {
                self.value = addition(value_from_storage, self.value, self.limit)?;
            },
            AggregatorState::NegativeDelta => {
                self.value = subtraction(value_from_storage, self.value)?;
            },
            AggregatorState::Data => {
                unreachable!("Materialization only happens in Delta state")
            },
        }

        // Change the state and return the new value. Also, make
        // sure history is no longer tracked.
        self.state = AggregatorState::Data;
        self.history = None;
        Ok(self.value)
    }

    /// Unpacks aggregator into its fields.
    pub fn into(self) -> (u128, AggregatorState, u128, Option<History>) {
        (self.value, self.state, self.limit, self.history)
    }
}

/// Stores all information about aggregators (how many have been created or
/// removed), what are their states, etc. per single transaction).
#[derive(Default)]
pub struct AggregatorData {
    // All aggregators that were created in the current transaction, stored as ids.
    // Used to filter out aggregators that were created and destroyed in the
    // within a single transaction.
    new_aggregators: BTreeSet<AggregatorID>,
    // All aggregators that were destroyed in the current transaction, stored as ids.
    destroyed_aggregators: BTreeSet<AggregatorID>,
    // All aggregator instances that exist in the current transaction.
    aggregators: BTreeMap<AggregatorID, Aggregator>,
    // Counter for generating identifiers for AggregatorSnapshots.
    pub id_counter: u64,
}

impl AggregatorData {
    pub fn new(id_counter: u64) -> Self {
        Self {
            id_counter,
            ..Default::default()
        }
    }

    /// Returns a mutable reference to an aggregator with `id` and a `limit`.
    /// If transaction that is currently executing did not initialize it, a new aggregator instance is created.
    /// Note: when we say "aggregator instance" here we refer to Rust struct and
    /// not to the Move aggregator.
    pub fn get_aggregator(
        &mut self,
        id: AggregatorID,
        limit: u128,
    ) -> PartialVMResult<&mut Aggregator> {
        let aggregator = self.aggregators.entry(id).or_insert(Aggregator {
            value: 0,
            state: AggregatorState::PositiveDelta,
            limit,
            history: Some(History::new()),
        });
        Ok(aggregator)
    }

    /// Returns the number of aggregators that are used in the current transaction.
    pub fn num_aggregators(&self) -> u128 {
        self.aggregators.len() as u128
    }

    /// Creates and a new Aggregator with a given `id` and a `limit`. The value
    /// of a new aggregator is always known, therefore it is created in a data
    /// state, with a zero-initialized value.
    pub fn create_new_aggregator(&mut self, id: AggregatorID, limit: u128) {
        let aggregator = Aggregator {
            value: 0,
            state: AggregatorState::Data,
            limit,
            history: None,
        };
        self.aggregators.insert(id, aggregator);
        self.new_aggregators.insert(id);
    }

    /// If aggregator has been used in this transaction, it is removed. Otherwise,
    /// it is marked for deletion.
    pub fn remove_aggregator(&mut self, id: AggregatorID) {
        // Aggregator no longer in use during this transaction: remove it.
        self.aggregators.remove(&id);

        if self.new_aggregators.contains(&id) {
            // Aggregator has been created in the same transaction. Therefore, no
            // side-effects.
            self.new_aggregators.remove(&id);
        } else {
            // Otherwise, aggregator has been created somewhere else.
            self.destroyed_aggregators.insert(id);
        }
    }

    pub fn generate_id(&mut self) -> u64 {
        self.id_counter += 1;
        self.id_counter
    }

    /// Unpacks aggregator data.
    pub fn into(
        self,
    ) -> (
        BTreeSet<AggregatorID>,
        BTreeSet<AggregatorID>,
        BTreeMap<AggregatorID, Aggregator>,
    ) {
        (
            self.new_aggregators,
            self.destroyed_aggregators,
            self.aggregators,
        )
    }
}

/// Returns partial VM error on extension failure.
pub fn extension_error(message: impl ToString) -> PartialVMError {
    PartialVMError::new(StatusCode::VM_EXTENSION_ERROR).with_message(message.to_string())
}

// ================================= Tests =================================

#[cfg(test)]
mod test {
    use super::*;
    use crate::{aggregator_id_for_test, AggregatorStore};
    use claims::{assert_err, assert_ok};
    use once_cell::sync::Lazy;

    #[allow(clippy::redundant_closure)]
    static TEST_RESOLVER: Lazy<AggregatorStore> = Lazy::new(|| AggregatorStore::default());

    #[test]
    fn test_materialize_not_in_storage() {
        let mut aggregator_data = AggregatorData::default();

        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(300), 700)
            .expect("Get aggregator failed");
        assert_err!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(700)));
    }

    #[test]
    fn test_materialize_known() {
        let mut aggregator_data = AggregatorData::default();
        aggregator_data.create_new_aggregator(aggregator_id_for_test(200), 200);

        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(200), 200)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_add(100));
        assert_ok!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(200)));
        assert_eq!(aggregator.value, 100);
    }

    #[test]
    fn test_materialize_overflow() {
        let mut aggregator_data = AggregatorData::default();

        // +0 to +400 satisfies <= 600 and is ok, but materialization fails
        // with 300 + 400 > 600!
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_add(400));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            400
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            None
        );
        assert_err!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(600)));
    }

    #[test]
    fn test_materialize_underflow() {
        let mut aggregator_data = AggregatorData::default();

        // +0 to -400 is ok, but materialization fails with 300 - 400 < 0!
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_sub(400));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            400
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            None
        );
        assert_err!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(600)));
    }

    #[test]
    fn test_materialize_non_monotonic_1() {
        let mut aggregator_data = AggregatorData::default();

        // +0 to +400 to +0 is ok, but materialization fails since we had 300 + 400 > 600!
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_add(400));
        assert_ok!(aggregator.try_sub(300));
        assert_eq!(aggregator.value, 100);
        assert_eq!(aggregator.state, AggregatorState::PositiveDelta);
        assert_err!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(600)));
    }

    #[test]
    fn test_materialize_non_monotonic_2() {
        let mut aggregator_data = AggregatorData::default();

        // +0 to -301 to -300 is ok, but materialization fails since we had 300 - 301 < 0!
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_sub(301));
        assert_ok!(aggregator.try_add(1));
        assert_eq!(aggregator.value, 300);
        assert_eq!(aggregator.state, AggregatorState::NegativeDelta);
        assert_err!(aggregator.read_and_materialize(&*TEST_RESOLVER, &aggregator_id_for_test(600)));
    }

    #[test]
    fn test_add_overflow() {
        let mut aggregator_data = AggregatorData::default();

        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");

        // +0 to +800 > 600!
        assert_err!(aggregator.try_add(800));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            0
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(800)
        );

        // +0 + 300 < 600
        assert_ok!(aggregator.try_add(300));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            300
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(800)
        );

        // +300 + 400 > 600!x
        assert_err!(aggregator.try_add(400));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            300
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(700)
        );

        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(200), 200)
            .expect("Get aggregator failed");

        // 0 + 100 < 200
        assert_ok!(aggregator.try_add(100));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            None
        );

        // 100 + 200 > 200!
        assert_err!(aggregator.try_add(200));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(300)
        );

        // 100 + 150 > 200!
        assert_err!(aggregator.try_add(150));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(250)
        );

        // 100 + u128::MAX > 200!
        assert_err!(aggregator.try_add(u128::MAX));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(250)
        );

        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(300), 300)
            .expect("Get aggregator failed");

        // 0 + 100 < 300!
        assert_ok!(aggregator.try_add(100));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            None
        );

        // 100 + u128::MAX > 300!
        assert_err!(aggregator.try_add(u128::MAX));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            None
        );

        // 100 + 250 > 300!
        assert_err!(aggregator.try_add(250));
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_overflow_positive,
            Some(350)
        );
    }

    #[test]
    fn test_sub_underflow() {
        let mut aggregator_data = AggregatorData::default();
        aggregator_data.create_new_aggregator(aggregator_id_for_test(200), 200);

        // +0 to -601 is impossible!
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_err!(aggregator.try_sub(700));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            0
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            Some(700)
        );

        assert_ok!(aggregator.try_add(200));

        assert_ok!(aggregator.try_sub(300));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            Some(700)
        );

        assert_err!(aggregator.try_sub(550));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            Some(650)
        );

        assert_err!(aggregator.try_sub(800));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            Some(650)
        );

        // Similarly, we cannot subtract anything from 0...
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(200), 200)
            .expect("Get aggregator failed");

        assert_err!(aggregator.try_sub(2));

        // Similarly, we cannot subtract anything from 0...
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(300), 300)
            .expect("Get aggregator failed");

        assert_ok!(aggregator.try_sub(100));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );

        assert_err!(aggregator.try_sub(u128::MAX));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            None
        );

        assert_ok!(aggregator.try_sub(100));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            200
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            None
        );

        assert_err!(aggregator.try_sub(101));
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            200
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_underflow_negative,
            Some(301)
        );
    }

    #[test]
    fn test_commutative() {
        let mut aggregator_data = AggregatorData::default();

        // +200 -300 +50 +300 -25 +375 -600.
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(600), 600)
            .expect("Get aggregator failed");
        assert_ok!(aggregator.try_add(200));
        assert_ok!(aggregator.try_sub(300));

        assert_eq!(aggregator.value, 100);
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            200
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(aggregator.state, AggregatorState::NegativeDelta);

        assert_ok!(aggregator.try_add(50));
        assert_ok!(aggregator.try_add(300));
        assert_ok!(aggregator.try_sub(25));

        assert_eq!(aggregator.value, 225);
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            250
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(aggregator.state, AggregatorState::PositiveDelta);

        assert_ok!(aggregator.try_add(375));
        assert_ok!(aggregator.try_sub(600));

        assert_eq!(aggregator.value, 0);
        assert_eq!(
            aggregator.history.as_ref().unwrap().max_achieved_positive,
            600
        );
        assert_eq!(
            aggregator.history.as_ref().unwrap().min_achieved_negative,
            100
        );
        assert_eq!(aggregator.state, AggregatorState::PositiveDelta);
    }

    #[test]
    #[should_panic]
    fn test_history_validation_in_data_state() {
        let mut aggregator_data = AggregatorData::default();

        // Validation panics if history is not set. This is an invariant
        // violation and should never happen.
        aggregator_data.create_new_aggregator(aggregator_id_for_test(200), 200);
        let aggregator = aggregator_data
            .get_aggregator(aggregator_id_for_test(200), 200)
            .expect("Getting an aggregator should succeed");
        aggregator
            .validate_history(0)
            .expect("Should not be called because validation panics");
    }

    #[test]
    fn test_history_validation_in_delta_state() {
        let mut aggregator_data = AggregatorData::default();

        // Some aggregator with a limit of 100 in a delta state.
        let id = aggregator_id_for_test(100);
        let aggregator = aggregator_data
            .get_aggregator(id, 100)
            .expect("Getting an aggregator should succeed");

        // Aggregator of +0 with minimum of -50 and maximum of +50.
        aggregator.try_add(50).unwrap();
        aggregator.try_sub(100).unwrap();
        aggregator.try_add(50).unwrap();

        // Valid history: 50+50-100+50.
        assert_ok!(aggregator.validate_history(50));

        // Underflow and overflow are unvalidated.
        assert_err!(aggregator.validate_history(49));
        assert_err!(aggregator.validate_history(51));
    }
}
