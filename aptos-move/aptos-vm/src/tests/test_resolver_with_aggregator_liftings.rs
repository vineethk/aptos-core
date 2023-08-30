// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::tests::mock_view::MockStateView;
use aptos_table_natives::{TableHandle, TableResolver};
use aptos_types::{access_path::AccessPath, state_store::state_key::StateKey};
use move_core_types::{
    account_address::AccountAddress,
    language_storage::StructTag,
    resolver::ResourceResolver,
    value::{LayoutTag, MoveStructLayout, MoveTypeLayout},
};
use move_vm_types::values::{Struct, Value};
use once_cell::sync::Lazy;
use std::{clone::Clone, str::FromStr};

macro_rules! test_struct {
    ($a:expr, $b:expr, $c:expr, $d:expr) => {
        Value::struct_(Struct::pack(vec![
            Value::u64($a),
            Value::u64($b),
            Value::u128($c),
            Value::u128($d),
        ]))
    };
}
static TEST_LAYOUT: Lazy<MoveTypeLayout> = Lazy::new(|| {
    MoveTypeLayout::Struct(MoveStructLayout::Runtime(vec![
        MoveTypeLayout::U64,
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U64)),
        MoveTypeLayout::U128,
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U128)),
    ]))
});

const TEST_ADDRESS: AccountAddress = AccountAddress::ONE;
static TEST_RESOURCE_TAG: Lazy<StructTag> =
    Lazy::new(|| StructTag::from_str("0x1::foo::Foo").unwrap());
static TEST_RESOURCE_STATE_KEY: Lazy<StateKey> = Lazy::new(|| {
    StateKey::access_path(
        AccessPath::resource_access_path(TEST_ADDRESS, (*TEST_RESOURCE_TAG).clone()).unwrap(),
    )
});

const TEST_TABLE_HANDLE: TableHandle = TableHandle(TEST_ADDRESS);
const TEST_TABLE_KEY: [u8; 32] = [9u8; 32];
static TEST_TABLE_ITEM_STATE_KEY: Lazy<StateKey> =
    Lazy::new(|| StateKey::table_item(TEST_TABLE_HANDLE.into(), TEST_TABLE_KEY.to_vec()));

#[test]
fn test_resource_in_storage() {
    let mut view = MockStateView::default();
    let test_struct = test_struct!(100, 200, 300, 400);
    view.add_to_db(
        (*TEST_RESOURCE_STATE_KEY).clone(),
        test_struct,
        (*TEST_LAYOUT).clone(),
    );

    let (blob, _) = view
        .get_resource_bytes_with_metadata(&TEST_ADDRESS, &TEST_RESOURCE_TAG, &[])
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 200, 300, 400);
    assert!(actual_value.equals(&expected_value).unwrap());

    let (blob, _) = view
        .get_resource_value_with_metadata(&TEST_ADDRESS, &TEST_RESOURCE_TAG, &[], &TEST_LAYOUT)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());
    view.assert_lifted_equal_at(0, Value::u64(200));
    view.assert_lifted_equal_at(1, Value::u128(400));
}

#[test]
fn test_table_item_in_storage() {
    let mut view = MockStateView::default();
    let test_struct = test_struct!(100, 200, 300, 400);
    view.add_to_db(
        (*TEST_TABLE_ITEM_STATE_KEY).clone(),
        test_struct,
        (*TEST_LAYOUT).clone(),
    );

    let blob = view
        .resolve_table_entry_bytes(&TEST_TABLE_HANDLE, &TEST_TABLE_KEY)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 200, 300, 400);
    assert!(actual_value.equals(&expected_value).unwrap());

    let blob = view
        .resolve_table_entry_value(&TEST_TABLE_HANDLE, &TEST_TABLE_KEY, &TEST_LAYOUT)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());
    view.assert_lifted_equal_at(0, Value::u64(200));
    view.assert_lifted_equal_at(1, Value::u128(400));
}

#[test]
fn test_resource_in_memory_cache() {
    let mut view = MockStateView::default();
    let test_struct = test_struct!(100, 0, 300, 1);
    view.add_to_in_memory_cache(
        (*TEST_RESOURCE_STATE_KEY).clone(),
        test_struct,
        (*TEST_LAYOUT).clone(),
    );
    view.add_lifting(0, Value::u64(200));
    view.add_lifting(1, Value::u128(400));
    view.assert_lifted_equal_at(0, Value::u64(200));
    view.assert_lifted_equal_at(1, Value::u128(400));

    let (blob, _) = view
        .get_resource_bytes_with_metadata(&TEST_ADDRESS, &TEST_RESOURCE_TAG, &[])
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());

    let (blob, _) = view
        .get_resource_value_with_metadata(&TEST_ADDRESS, &TEST_RESOURCE_TAG, &[], &TEST_LAYOUT)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());
}

#[test]
fn test_table_item_in_memory_cache() {
    let mut view = MockStateView::default();
    let test_struct = test_struct!(100, 0, 300, 1);
    view.add_to_in_memory_cache(
        (*TEST_TABLE_ITEM_STATE_KEY).clone(),
        test_struct,
        (*TEST_LAYOUT).clone(),
    );
    view.add_lifting(0, Value::u64(200));
    view.add_lifting(1, Value::u128(400));
    view.assert_lifted_equal_at(0, Value::u64(200));
    view.assert_lifted_equal_at(1, Value::u128(400));

    let blob = view
        .resolve_table_entry_bytes(&TEST_TABLE_HANDLE, &TEST_TABLE_KEY)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());

    let blob = view
        .resolve_table_entry_value(&TEST_TABLE_HANDLE, &TEST_TABLE_KEY, &TEST_LAYOUT)
        .unwrap();
    let actual_value = Value::simple_deserialize(&blob.unwrap(), &TEST_LAYOUT).unwrap();
    let expected_value = test_struct!(100, 0, 300, 1);
    assert!(actual_value.equals(&expected_value).unwrap());
}
