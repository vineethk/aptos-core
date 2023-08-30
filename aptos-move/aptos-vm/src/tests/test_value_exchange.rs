// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::tests::mock_view::MockStateView;
use claims::assert_none;
use move_core_types::value::{LayoutTag, MoveStructLayout::Runtime, MoveTypeLayout};
use move_vm_types::{
    value_transformation::{deserialize_and_exchange, serialize_and_exchange},
    values::{Struct, Value},
};

#[test]
fn test_exchange_not_supported() {
    let exchange = MockStateView::default();

    // We cannot exchange non u64/u128 types.
    let layout =
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::Bool));
    let input_value = Value::bool(false);
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    assert_none!(deserialize_and_exchange(&input_blob, &layout, &exchange));

    // Inner types in vector layouts cannot be tagged.
    let layout = MoveTypeLayout::Vector(Box::new(MoveTypeLayout::Tagged(
        LayoutTag::AggregatorLifting,
        Box::new(MoveTypeLayout::U64),
    )));
    let input_value = Value::vector_u64(vec![1, 2, 3]);
    assert_none!(input_value.simple_serialize(&layout));

    // But also tagging all vector is not supported.
    let layout = MoveTypeLayout::Tagged(
        LayoutTag::AggregatorLifting,
        Box::new(MoveTypeLayout::Vector(Box::new(MoveTypeLayout::U64))),
    );
    let input_value = Value::vector_u64(vec![1, 2, 3]);
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    assert_none!(deserialize_and_exchange(&input_blob, &layout, &exchange));

    let layout = MoveTypeLayout::Tagged(
        LayoutTag::AggregatorLifting,
        Box::new(MoveTypeLayout::Struct(Runtime(vec![]))),
    );
    let input_value = Value::struct_(Struct::pack(vec![]));
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    assert_none!(deserialize_and_exchange(&input_blob, &layout, &exchange));
}

#[test]
fn test_exchange_preserves_value() {
    let exchange = MockStateView::default();

    let layout = MoveTypeLayout::U64;
    let input_value = Value::u64(100);
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    let patched_value = deserialize_and_exchange(&input_blob, &layout, &exchange).unwrap();
    let unpatched_value = Value::simple_deserialize(
        &serialize_and_exchange(&patched_value, &layout, &exchange).unwrap(),
        &layout,
    )
    .unwrap();
    assert!(patched_value.equals(&Value::u64(100)).unwrap());
    assert!(unpatched_value.equals(&input_value).unwrap());
}

#[test]
fn test_exchange_u64() {
    let exchange = MockStateView::default();

    let layout =
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U64));
    let input_value = Value::u64(200);
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    let patched_value = deserialize_and_exchange(&input_blob, &layout, &exchange).unwrap();
    let unpatched_value = Value::simple_deserialize(
        &serialize_and_exchange(&patched_value, &layout, &exchange).unwrap(),
        &layout,
    )
    .unwrap();
    exchange.assert_lifted_equal_at(0, Value::u64(200));
    assert!(patched_value.equals(&Value::u64(0)).unwrap());
    assert!(unpatched_value.equals(&input_value).unwrap());
}

#[test]
fn test_exchange_u128() {
    let exchange = MockStateView::default();

    let layout =
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U128));
    let input_value = Value::u128(300);
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    let patched_value = deserialize_and_exchange(&input_blob, &layout, &exchange).unwrap();
    let unpatched_value = Value::simple_deserialize(
        &serialize_and_exchange(&patched_value, &layout, &exchange).unwrap(),
        &layout,
    )
    .unwrap();
    exchange.assert_lifted_equal_at(0, Value::u128(300));
    assert!(patched_value.equals(&Value::u128(0)).unwrap());
    assert!(unpatched_value.equals(&input_value).unwrap());
}

#[test]
fn test_exchange_works_inside_struct() {
    let exchange = MockStateView::default();

    let layout = MoveTypeLayout::Struct(Runtime(vec![
        MoveTypeLayout::U64,
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U64)),
        MoveTypeLayout::Tagged(LayoutTag::AggregatorLifting, Box::new(MoveTypeLayout::U128)),
    ]));

    let input_value = Value::struct_(Struct::pack(vec![
        Value::u64(400),
        Value::u64(500),
        Value::u128(600),
    ]));
    let input_blob = input_value.simple_serialize(&layout).unwrap();
    let patched_value = deserialize_and_exchange(&input_blob, &layout, &exchange).unwrap();
    let unpatched_value = Value::simple_deserialize(
        &serialize_and_exchange(&patched_value, &layout, &exchange).unwrap(),
        &layout,
    )
    .unwrap();
    exchange.assert_lifted_equal_at(0, Value::u64(500));
    exchange.assert_lifted_equal_at(1, Value::u128(600));
    let expected_patched_value = Value::struct_(Struct::pack(vec![
        Value::u64(400),
        Value::u64(0),
        Value::u128(1),
    ]));
    assert!(patched_value.equals(&expected_patched_value).unwrap());
    assert!(unpatched_value.equals(&input_value).unwrap());
}
