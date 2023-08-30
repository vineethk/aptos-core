// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

//! This file implements traits for WebAuthn based ECDSA signatures over NIST-P256.

use crate::{
    hash::CryptoHash,
    p256::{P256PrivateKey, P256PublicKey, ORDER_HALF},
    traits::*,
};
use anyhow::{anyhow, Result};
use aptos_crypto_derive::{DeserializeKey, SerializeKey};
use core::convert::TryFrom;
use p256::{elliptic_curve::Curve, NistP256, NonZeroScalar};
use serde::Serialize;
use signature::Verifier;
use std::{cmp::Ordering, fmt};

/// A WebAuthn P256 signature
/// TODO: This will not compose p256::ecdsa::Signature
#[derive(DeserializeKey, Clone, SerializeKey)]
pub struct WebAuthnP256Signature(pub(crate) p256::ecdsa::Signature);
