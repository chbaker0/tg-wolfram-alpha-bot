#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct EyreWrapper(#[from] pub eyre::Report);
