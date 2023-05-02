#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct EyreWrapper(#[from] pub eyre::Report);

impl EyreWrapper {
    pub fn from_boxed_error(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::from(eyre::eyre!(e))
    }
}
