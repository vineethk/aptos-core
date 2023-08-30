// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{assert_success, tests::common::test_dir_path, MoveHarness};
use aptos_language_e2e_tests::account::Account;
use aptos_types::{on_chain_config::FeatureFlag, write_set::WriteOp};
use aptos_vm::testing::{testing_only::inject_error_once, InjectedError};
use move_core_types::account_address::AccountAddress;
use serde::Serialize;

#[test]
fn test_refunds() {
    let mut h = MoveHarness::new_with_features(
        vec![
            FeatureFlag::STORAGE_SLOT_METADATA,
            FeatureFlag::MODULE_EVENT,
            FeatureFlag::EMIT_FEE_STATEMENT,
            FeatureFlag::STORAGE_DELETION_REFUND,
        ],
        vec![],
    );
    let mod_addr = AccountAddress::from_hex_literal("0xcafe").unwrap();
    let user_addr = AccountAddress::from_hex_literal("0x100").unwrap();
    let mod_acc = h.new_account_at(mod_addr);
    let user_acc = h.new_account_at(user_addr);

    assert_success!(h.publish_package(&mod_acc, &test_dir_path("storage_refund.data/pack")));

    // store a resource under 0xcafe
    assert_succ(&mut h, &mod_acc, "store_resource_to", vec![], 1);

    // 0x100 removes it
    let args = vec![ser(&mod_addr)];
    assert_succ(&mut h, &user_acc, "remove_resource_from", args, -1);

    // initialize global stack and push a few items
    assert_succ(&mut h, &mod_acc, "init_stack", vec![], 1);
    assert_succ(&mut h, &user_acc, "stack_push", vec![ser(&10u64)], 10);

    // pop stack items and assert refund amount
    assert_succ(&mut h, &user_acc, "stack_pop", vec![ser(&2u64)], -2);
    assert_succ(&mut h, &mod_acc, "stack_pop", vec![ser(&5u64)], -5);

    // Inject error in epilogue, observe refund is not applied (slot allocation is still charged.)
    // (need to disable parallel execution)
    inject_error_once(InjectedError::EndOfRunEpilogue);
    assert_result(&mut h, &mod_acc, "store_1_pop_2", vec![], 1, false);

    // Same thing is expected to succeed without injected error. (two slots freed, net refund for 1 slot)
    assert_succ(&mut h, &mod_acc, "store_1_pop_2", vec![], -1);

    // Create many slots (with SmartTable)
    assert_succ(&mut h, &user_acc, "init_collection_of_1000", vec![], 135);

    // Release many slots.
    assert_succ(&mut h, &user_acc, "destroy_collection", vec![], -135);
}

const LEEWAY: u64 = 2000;

fn read_slot_fee_from_gas_schedule(h: &MoveHarness) -> u64 {
    let slot_fee = h
        .new_vm()
        .internals()
        .gas_params()
        .unwrap()
        .vm
        .txn
        .storage_fee_per_state_slot_create
        .into();
    assert!(slot_fee > 0);
    assert!(slot_fee > LEEWAY * 10);
    slot_fee
}

fn ser<T: Serialize>(t: &T) -> Vec<u8> {
    bcs::to_bytes(t).unwrap()
}

fn assert_succ(
    h: &mut MoveHarness,
    account: &Account,
    fun: &str,
    args: Vec<Vec<u8>>,
    expect_num_slots_charged: i64, // negative for refund
) {
    assert_result(h, account, fun, args, expect_num_slots_charged, true);
}

fn assert_result(
    h: &mut MoveHarness,
    account: &Account,
    fun: &str,
    args: Vec<Vec<u8>>,
    expect_num_slots_charged: i64, // negative for refund
    expect_success: bool,
) {
    let start_balance = h.read_aptos_balance(account.address());

    // run the function
    let txn = h.create_entry_function(
        account,
        format!("0xcafe::test::{}", fun).parse().unwrap(),
        vec![],
        args,
    );
    let gas_unit_price = txn.gas_unit_price();
    assert!(gas_unit_price > 0);
    let txn_out = h.run_raw(txn);
    if expect_success {
        assert_success!(*txn_out.status());
    }

    let end_balance = h.read_aptos_balance(account.address());

    // check the creates / deletes in the txn output
    let mut creates = 0;
    let mut deletes = 0;
    for (_state_key, write_op) in txn_out.write_set() {
        match write_op {
            WriteOp::CreationWithMetadata { .. } | WriteOp::Creation(_) => creates += 1,
            WriteOp::DeletionWithMetadata { .. } => deletes += 1,
            WriteOp::Deletion => panic!("This test expects all deletions to have metadata"),
            WriteOp::Modification(_) | WriteOp::ModificationWithMetadata { .. } => (),
        }
    }
    if expect_success {
        assert_eq!(creates - deletes, expect_num_slots_charged);
    } else {
        assert!(expect_num_slots_charged >= creates);
    }

    // check the balance
    let slot_fee = read_slot_fee_from_gas_schedule(h);
    let expected_end = (start_balance as i64 - slot_fee as i64 * expect_num_slots_charged) as u64;
    let leeway = LEEWAY * expect_num_slots_charged.unsigned_abs();
    assert!(expected_end + leeway > end_balance);
    assert!(expected_end < end_balance + leeway);

    // check the fee statement
    let fee_statement = txn_out.try_extract_fee_statement().unwrap().unwrap();
    let diff_from_fee_statement = fee_statement.storage_fee_refund() as i64
        - (fee_statement.gas_used() * gas_unit_price) as i64;
    assert_eq!(
        diff_from_fee_statement,
        end_balance as i64 - start_balance as i64
    );
}
