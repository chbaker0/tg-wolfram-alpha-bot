#[derive(Debug, thiserror::Error)]
#[error("{inner:?}")]
pub struct EyreWrapper {
    #[from]
    inner: eyre::Report,
}
