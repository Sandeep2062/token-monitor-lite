//! Exact counters. Provider readers determine inclusive/exclusive semantics;
//! a universal component sum would double-count some reasoning observations.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub client: String,
    pub session: String,
}

impl SessionKey {
    pub fn new(client: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            client: client.into(),
            session: session.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenDelta {
    pub input: i128,
    pub output: i128,
    pub cache_read: i128,
    pub cache_write: i128,
    pub reasoning: i128,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AccountingError {
    #[error("token count overflow in {0}")]
    Overflow(&'static str),
}

impl TokenCounts {
    pub fn checked_add(self, other: Self) -> Result<Self, AccountingError> {
        Ok(Self {
            input: self
                .input
                .checked_add(other.input)
                .ok_or(AccountingError::Overflow("input"))?,
            output: self
                .output
                .checked_add(other.output)
                .ok_or(AccountingError::Overflow("output"))?,
            cache_read: self
                .cache_read
                .checked_add(other.cache_read)
                .ok_or(AccountingError::Overflow("cache read"))?,
            cache_write: self
                .cache_write
                .checked_add(other.cache_write)
                .ok_or(AccountingError::Overflow("cache write"))?,
            reasoning: self
                .reasoning
                .checked_add(other.reasoning)
                .ok_or(AccountingError::Overflow("reasoning"))?,
        })
    }

    pub fn difference(self, previous: Self) -> TokenDelta {
        TokenDelta {
            input: i128::from(self.input) - i128::from(previous.input),
            output: i128::from(self.output) - i128::from(previous.output),
            cache_read: i128::from(self.cache_read) - i128::from(previous.cache_read),
            cache_write: i128::from(self.cache_write) - i128::from(previous.cache_write),
            reasoning: i128::from(self.reasoning) - i128::from(previous.reasoning),
        }
    }
}
