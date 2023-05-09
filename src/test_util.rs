#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct EyreWrapper(#[from] pub eyre::Report);

/// "unwraps" `Option<T>` in an async context, never resolving if `None`. Useful
/// in `select!` (e.g. `fut.then(or_pending)`).
pub fn or_pending<T>(
    v: Option<T>,
) -> futures::future::Either<std::future::Ready<T>, std::future::Pending<T>> {
    match v {
        Some(v) => futures::future::Either::Left(std::future::ready(v)),
        None => futures::future::Either::Right(std::future::pending()),
    }
}
