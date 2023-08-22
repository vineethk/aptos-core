// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T, E = Error> = ::std::result::Result<T, E>;

/// An error returned by the Aptos Data Client for failed API calls.
#[derive(Clone, Debug, Deserialize, Error, PartialEq, Eq, Serialize)]
pub enum Error {
    #[error("The requested data is unavailable and cannot be found! Error: {0}")]
    DataIsUnavailable(String),
    #[error("The requested data is too large: {0}")]
    DataIsTooLarge(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("Timed out waiting for a response: {0}")]
    TimeoutWaitingForResponse(String),
    #[error("Unexpected error encountered: {0}")]
    UnexpectedErrorEncountered(String),
}

impl Error {
    /// Returns a summary label for the error
    pub fn get_label(&self) -> &'static str {
        match self {
            Self::DataIsUnavailable(_) => "data_is_unavailable",
            Self::DataIsTooLarge(_) => "data_is_too_large",
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidResponse(_) => "invalid_response",
            Self::TimeoutWaitingForResponse(_) => "timeout_waiting_for_response",
            Self::UnexpectedErrorEncountered(_) => "unexpected_error_encountered",
        }
    }

    /// Returns true iff the error is a timeout error
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::TimeoutWaitingForResponse(_))
    }
}

impl From<aptos_storage_service_client::Error> for Error {
    fn from(error: aptos_storage_service_client::Error) -> Self {
        Self::UnexpectedErrorEncountered(error.to_string())
    }
}

impl From<aptos_storage_service_types::responses::Error> for Error {
    fn from(error: aptos_storage_service_types::responses::Error) -> Self {
        Self::InvalidResponse(error.to_string())
    }
}
