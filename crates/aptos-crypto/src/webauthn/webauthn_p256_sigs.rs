// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

//! This file implements traits for ECDSA signatures over NIST-P256.

use crate::{
    hash::CryptoHash,
    p256::P256Signature,
    traits::*,
    webauthn::{webauthn_p256_keys::WebAuthnP256PrivateKey, WebAuthnP256PublicKey},
};
use anyhow::{anyhow, Result};
use aptos_crypto_derive::{DeserializeKey, SerializeKey};
use core::convert::TryFrom;
use serde::Serialize;
use signature::Verifier;
use std::fmt;

/// A WebAuthn P256 signature
#[derive(DeserializeKey, Clone, SerializeKey)]
pub struct WebAuthnP256Signature(pub(crate) P256Signature);

impl private::Sealed for WebAuthnP256Signature {}

impl WebAuthnP256Signature {
    /// The length of the WebAuthnP256Signature
    pub const LENGTH: usize = P256Signature::LENGTH;

    /// Serialize an WebAuthnP256Signature.
    pub fn to_bytes(&self) -> [u8; P256Signature::LENGTH] {
        // The RustCrypto P256 `to_bytes` call here should never return a byte array of the wrong length
        self.0.to_bytes()
    }

    /// Deserialize an WebAuthnP256Signature, without checking for malleability
    pub(crate) fn from_bytes_unchecked(
        bytes: &[u8],
    ) -> std::result::Result<WebAuthnP256Signature, CryptoMaterialError> {
        match P256Signature::try_from(bytes) {
            Ok(p256_signature) => Ok(WebAuthnP256Signature(p256_signature)),
            Err(_) => Err(CryptoMaterialError::DeserializationError),
        }
    }

    /// return an all-zero signature (for test only)
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn dummy_signature() -> Self {
        Self::from_bytes_unchecked(&[0u8; Self::LENGTH]).unwrap()
    }

    /// See P256Signature::check_s_malleability
    pub fn check_s_malleability(bytes: &[u8]) -> std::result::Result<(), CryptoMaterialError> {
        P256Signature::check_s_malleability(bytes)
    }

    /// If the signature {R,S} does not have S < n/2 where n is the Ristretto255 order, return
    /// {R,n-S} as the canonical encoding of this signature to prevent malleability attacks. See
    /// `check_s_malleability` for more detail
    pub fn make_canonical(&self) -> WebAuthnP256Signature {
        let signature = P256Signature::make_canonical(&self.0);
        WebAuthnP256Signature(signature)
    }
}

//////////////////////
// Signature Traits //
//////////////////////

impl Signature for WebAuthnP256Signature {
    type SigningKeyMaterial = WebAuthnP256PrivateKey;
    type VerifyingKeyMaterial = WebAuthnP256PublicKey;

    /// Verifies that the provided signature is valid for the provided message, going beyond the
    /// [NIST SP 800-186](https://csrc.nist.gov/publications/detail/sp/800-186/final) specification, to prevent scalar malleability as done in [BIP146](https://github.com/bitcoin/bips/blob/master/bip-0146.mediawiki).
    fn verify<T: CryptoHash + Serialize>(
        &self,
        message: &T,
        public_key: &WebAuthnP256PublicKey,
    ) -> Result<()> {
        Self::verify_arbitrary_msg(self, &signing_message(message)?, public_key)
    }

    /// Checks that `self` is valid for an arbitrary &[u8] `message` using `public_key`.
    /// Outside of this crate, this particular function should only be used for native signature
    /// verification in Move.
    ///
    /// This function will check both the signature and `public_key` for small subgroup attacks.
    fn verify_arbitrary_msg(
        &self,
        message: &[u8],
        public_key: &WebAuthnP256PublicKey,
    ) -> Result<()> {
        WebAuthnP256Signature::check_s_malleability(&self.to_bytes())?;

        public_key
            .0
             .0
            .verify(message, &self.0 .0)
            .map_err(|e| anyhow!("{}", e))
            .and(Ok(()))
    }

    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_bytes().to_vec()
    }
}

impl Length for WebAuthnP256Signature {
    fn length(&self) -> usize {
        P256Signature::LENGTH
    }
}

impl ValidCryptoMaterial for WebAuthnP256Signature {
    fn to_bytes(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }
}

impl std::hash::Hash for WebAuthnP256Signature {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let encoded_signature = self.to_bytes();
        state.write(&encoded_signature);
    }
}

impl TryFrom<&[u8]> for WebAuthnP256Signature {
    type Error = CryptoMaterialError;

    fn try_from(bytes: &[u8]) -> std::result::Result<WebAuthnP256Signature, CryptoMaterialError> {
        WebAuthnP256Signature::check_s_malleability(bytes)?;
        WebAuthnP256Signature::from_bytes_unchecked(bytes)
    }
}

// Those are required by the implementation of hash above
impl PartialEq for WebAuthnP256Signature {
    fn eq(&self, other: &WebAuthnP256Signature) -> bool {
        self.to_bytes()[..] == other.to_bytes()[..]
    }
}

impl Eq for WebAuthnP256Signature {}

impl fmt::Display for WebAuthnP256Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(&self.0.to_bytes()[..]))
    }
}

impl fmt::Debug for WebAuthnP256Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WebAuthnP256Signature({})", self)
    }
}
