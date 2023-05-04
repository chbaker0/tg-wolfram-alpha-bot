use std::task;

use futures::Stream;
use pin_project::pin_project;

pub fn stream_flatten_ok<S, T, E, I>(stream: S) -> StreamFlattenOk<I::IntoIter, S>
where
    S: Stream<Item = Result<I, E>>,
    I: IntoIterator<Item = T>,
{
    StreamFlattenOk { stream, cont: None }
}

#[pin_project]
pub struct StreamFlattenOk<Iter, S> {
    #[pin]
    stream: S,
    cont: Option<Iter>,
}

impl<T, E, I, S> Stream for StreamFlattenOk<I::IntoIter, S>
where
    S: Stream<Item = Result<I, E>>,
    I: IntoIterator<Item = T>,
{
    type Item = Result<T, E>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        use task::ready;
        use task::Poll::*;

        let mut this = self.project();

        loop {
            if let Some(next) = this.cont.as_mut().and_then(|i| i.next()) {
                return Ready(Some(Ok(next)));
            }

            match ready!(this.stream.as_mut().poll_next(cx)) {
                Some(Ok(cont)) => *this.cont = Some(cont.into_iter()),
                Some(Err(e)) => return Ready(Some(Err(e))),
                None => return Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            self.cont.as_ref().map(|c| c.size_hint().0).unwrap_or(0),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_test::{assert_stream_done, assert_stream_next, assert_stream_pending};
    use task::Poll;

    #[test]
    fn test_flatten_ok() {
        use futures::stream;
        use futures_test::stream::*;

        let input: Vec<Result<Vec<u32>, String>> = vec![
            Ok(vec![1, 2, 3]),
            Ok(vec![]),
            Err("foo".to_string()),
            Err("bar".to_string()),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![4]),
            Err("??".to_string()),
            Ok(vec![5, 6]),
        ];
        let under = stream::iter(input).interleave_pending().assert_unmoved();

        let test = stream_flatten_ok(under);
        futures::pin_mut!(test);
        assert_stream_pending!(test);
        assert_stream_next!(test, Ok(1));
        assert_stream_next!(test, Ok(2));
        assert_stream_next!(test, Ok(3));
        assert_stream_pending!(test);
        assert_stream_pending!(test);
        assert_stream_next!(test, Err("foo".to_string()));
        assert_stream_pending!(test);
        assert_stream_next!(test, Err("bar".to_string()));
        assert_stream_pending!(test);
        assert_stream_pending!(test);
        assert_stream_pending!(test);
        assert_stream_next!(test, Ok(4));
        assert_stream_pending!(test);
        assert_stream_next!(test, Err("??".to_string()));
        assert_stream_pending!(test);
        assert_stream_next!(test, Ok(5));
        assert_stream_next!(test, Ok(6));
        assert_stream_pending!(test);
        assert_stream_done!(test);
    }
}
