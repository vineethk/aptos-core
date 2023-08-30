// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0
//! This module provides an API for the WebAuthn account authenticator as defined in [W3C WebAuthn Level 2](https://www.w3.org/TR/webauthn-2/).
//!
//! # Examples
//! TODO: Coming soon
//! ```
//! use aptos_crypto_derive::{CryptoHasher, BCSCryptoHash};
//! use aptos_crypto::{
//!     p256::*,
//!     traits::{Signature, SigningKey, Uniform},
//!     test_utils::KeyPair
//! };
//! use rand::{rngs::StdRng, SeedableRng};
//! use rand_core::OsRng;
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, CryptoHasher, BCSCryptoHash)]
//! pub struct TestCryptoDocTest(String);
//! let message = TestCryptoDocTest("Test message".to_string());
//!
//! let mut rng = OsRng;
//! let kp = KeyPair::<P256PrivateKey, P256PublicKey>::generate(&mut rng);
//!
//! let signature = kp.private_key.sign(&message).unwrap();
//! assert!(signature.verify(&message, &kp.public_key).is_ok());
//! ```

pub mod webauthn_p256_keys;
pub mod webauthn_p256_sigs;

/// WebAuthn fields that are included in the WebAuthn Signature and necessary for
/// verification of the transaction
pub mod webauthn_fields;

pub use webauthn_p256_keys::{WebAuthnP256PublicKey, WebAuthnP256PublicKey as PublicKey};
pub use webauthn_p256_sigs::{WebAuthnP256Signature, WebAuthnP256Signature as Signature};
