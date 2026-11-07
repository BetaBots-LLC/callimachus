//! App-wide error type. Internal modules use `anyhow::Result`; Tauri commands
//! return `AppResult<T>`, which serializes the error to a string for the frontend.

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl AppError {
    /// The wrapped error with its whole chain, for code that continues in anyhow.
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            AppError::Other(e) => e,
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_inner_keeps_the_whole_chain() {
        let err: AppError = anyhow::anyhow!("disk I/O error")
            .context("writing the index")
            .into();
        // The old `anyhow!(e.to_string())` kept only the outer layer; logs print `{e:#}`.
        assert_eq!(
            format!("{:#}", err.into_inner()),
            "writing the index: disk I/O error"
        );
    }
}
