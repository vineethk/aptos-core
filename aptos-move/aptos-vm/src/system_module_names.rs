// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Names of modules, functions, and types used by Aptos System.

use aptos_types::account_config;
use move_core_types::{ident_str, identifier::IdentStr, language_storage::ModuleId};
use once_cell::sync::Lazy;

// Data to resolve basic account and transaction flow functions and structs
/// The ModuleId for the aptos block module
pub static BLOCK_MODULE: Lazy<ModuleId> = Lazy::new(|| {
    ModuleId::new(
        account_config::CORE_CODE_ADDRESS,
        ident_str!("block").to_owned(),
    )
});

// TZ: TODO: remove these except for the block-related names
// Names for special functions and structs
pub const SCRIPT_PROLOGUE_NAME: &IdentStr = ident_str!("script_prologue");
pub const MULTI_AGENT_SCRIPT_PROLOGUE_NAME: &IdentStr = ident_str!("multi_agent_script_prologue");
pub const MODULE_PROLOGUE_NAME: &IdentStr = ident_str!("module_prologue");
pub const USER_EPILOGUE_NAME: &IdentStr = ident_str!("epilogue");
pub const BLOCK_PROLOGUE: &IdentStr = ident_str!("block_prologue");

pub static MULTISIG_ACCOUNT_MODULE: Lazy<ModuleId> = Lazy::new(|| {
    ModuleId::new(
        account_config::CORE_CODE_ADDRESS,
        ident_str!("multisig_account").to_owned(),
    )
});
pub const VALIDATE_MULTISIG_TRANSACTION: &IdentStr = ident_str!("validate_multisig_transaction");
pub const GET_NEXT_TRANSACTION_PAYLOAD: &IdentStr = ident_str!("get_next_transaction_payload");
pub const SUCCESSFUL_TRANSACTION_EXECUTION_CLEANUP: &IdentStr =
    ident_str!("successful_transaction_execution_cleanup");
pub const FAILED_TRANSACTION_EXECUTION_CLEANUP: &IdentStr =
    ident_str!("failed_transaction_execution_cleanup");

pub static TRANSACTION_FEE_MODULE: Lazy<ModuleId> = Lazy::new(|| {
    ModuleId::new(
        account_config::CORE_CODE_ADDRESS,
        ident_str!("transaction_fee").to_owned(),
    )
});

pub const EMIT_FEE_STATEMENT: &IdentStr = ident_str!("emit_fee_statement");
