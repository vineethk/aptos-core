// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

/// This file contains the WebAuthn fields used for verifying a WebAuthn transaction
/// TODO: Fill in more
#[derive(Clone, Debug)]
pub struct WebAuthnFields {
    client_data_json: Vec<u8>,
    authenticator_data: Vec<u8>,
}
