// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;
use uc_types::ErrorCategory;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("resource limit: {0}")]
    Resource(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl ToolError {
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidInput(_) => ErrorCategory::InvalidInput,
            Self::PermissionDenied(_) => ErrorCategory::PermissionDenied,
            Self::Backend(_) => ErrorCategory::Backend,
            Self::Resource(_) => ErrorCategory::Resource,
            Self::Internal(_) => ErrorCategory::Internal,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "tool.invalid_input",
            Self::PermissionDenied(_) => "tool.permission_denied",
            Self::Backend(_) => "tool.backend",
            Self::Resource(_) => "tool.resource",
            Self::Internal(_) => "tool.internal",
        }
    }

    pub fn into_proto(self) -> uc_types::Error {
        uc_types::Error {
            category: self.category() as i32,
            code: self.code().to_string(),
            message: self.to_string(),
            details: Default::default(),
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::Backend(format!("io: {e}"))
    }
}

pub type ToolResult<T> = Result<T, ToolError>;
