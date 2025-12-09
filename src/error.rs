//! Custom error types for UBT ExEx.

use thiserror::Error;

/// Errors that can occur in UBT ExEx operations.
#[derive(Error, Debug)]
pub enum UbtError {
    #[error("Database error: {0}")]
    Database(#[from] DatabaseError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("State extraction error: {message}")]
    #[allow(dead_code)]
    StateExtraction { message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Root verification failed: expected {expected}, computed {computed}")]
    RootVerificationFailed { expected: String, computed: String },
}

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("MDBX error: {0}")]
    Mdbx(String),

    #[error("Failed to open database at {path}: {reason}")]
    Open { path: String, reason: String },

    #[error("Transaction failed: {0}")]
    Transaction(String),
}

pub type Result<T> = std::result::Result<T, UbtError>;
