#![feature(assert_matches)]

mod http_service;
mod telegram;
mod util;
mod wolfram;

use futures::Future;
use futures::TryStreamExt;
use telegram::Client;

use std::sync::Arc;

use eyre::Context;
use futures::FutureExt;
use futures::StreamExt;
use itertools::Itertools;
use tower::util::ServiceExt;
use tower::Service;
use tracing::{error, instrument};
use tracing_futures::Instrument;

use tokio::sync::mpsc as chan;

fn setup_tracing() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
        color_eyre::install().unwrap();

        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::fmt::fmt()
            .finish()
            .with(tracing_error::ErrorLayer::default())
            .init();
    });
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    setup_tracing();

    let _guard = tokio::task::LocalSet::new().enter();

    let client = http_service::Client::new(reqwest::Client::new());
    let wolf = Arc::new(wolfram::Api::new(client.clone(), WOLFRAM_KEY.trim_end()));
    let tg = telegram::Bot::new(TELEGRAM_KEY.trim_end()).on(client.map_response(|r| r.bytes));

    let me = tg.call(telegram::GetMe).await?;
    println!("ID: {}", me.id);
    eyre::ensure!(me.is_bot, "we're not a bot?");

    let (sender, mut receiver) = chan::channel(100);
    let tg_update_task = tokio::task::spawn({
        let tg = tg.clone();
        async move { update_streamer(tg, tokio_util::sync::PollSender::new(sender)).await }
    });

    while let Some(u) = receiver.recv().await {
        let Some(msg) = u.message else {continue};

        // Spawn and ignore the handle, since the task doesn't return anything and
        // logs any errors.
        handle_request(&tg, wolf.as_ref(), msg).await;
    }

    tg_update_task.await?
}

#[instrument(skip(tg, wolfram))]
async fn handle_request<E, Tg, Wolf>(tg: &Tg, wolfram: Wolf, msg: telegram::Message)
where
    Tg: telegram::Client,
    Tg::Error: std::error::Error + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
    Wolf: Service<String, Response = wolfram::SimpleResponse, Error = wolfram::ApiError<E>>,
{
    match handle_request_impl(tg, wolfram, &msg).await {
        Ok(()) => (),
        Err(report) => {
            error!(
                msg.text, msg.chat.id,
                report = ?report, "error handling Telegram query"
            );
        }
    }
}

async fn handle_request_impl<E, Tg, Wolf>(
    tg: &Tg,
    mut wolfram: Wolf,
    msg: &telegram::Message,
) -> eyre::Result<()>
where
    Tg: telegram::Client,
    Tg::Error: std::error::Error + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
    Wolf: Service<String, Response = wolfram::SimpleResponse, Error = wolfram::ApiError<E>>,
{
    tracing::info!(msg.text);
    let Some(mut text) = msg.text.as_deref() else {return Ok(())};

    if text.starts_with(['@', '/']) {
        match text.split_once(' ') {
            Some((_, r)) => text = r,
            None => return Ok(()),
        }
    }

    if text.is_empty() {
        return Ok(());
    }

    let send_message = |text| {
        tg.call(telegram::SendMessage {
            chat_id: msg.chat.id,
            reply_to_message_id: msg.message_id,
            text,
        })
    };

    match wolfram.call(text.to_string()).await {
        Ok(resp) => {
            if let Some(q) = telegram::SendPhoto::new(
                msg.chat.id,
                msg.message_id,
                resp.image_data,
                resp.content_type,
            ) {
                tg.call(q).await
            } else {
                send_message("Wolfram Alpha sent a bad image".to_string()).await
            }
        }
        Err(wolfram::ApiError::InvalidQuery) => {
            send_message("Wolfram Alpha could not process this query".to_string()).await
        }
        Err(_) => send_message("Could not contact Wolfram Alpha...try again?".to_string()).await,
    }
    .map(|_| ())
    .wrap_err("telegram api request failed")
}

async fn update_streamer<E, S, Sink>(tg: S, sink: Sink) -> eyre::Result<()>
where
    S: Service<
            telegram::GetUpdates,
            Response = Vec<telegram::Update>,
            Error = telegram::ApiError<E>,
        > + Clone,
    E: std::error::Error + Send + Sync + 'static,
    Sink: futures::Sink<telegram::Update> + Unpin,
    Sink::Error: std::error::Error + Send + Sync + 'static,
{
    let wrapped = tower::service_fn(|req: telegram::GetUpdates| {
        let mut tg = tg.clone();
        async move {
            loop {
                match (&mut tg).oneshot(req.clone()).await {
                    Ok(u) => {
                        return Ok(u);
                    }
                    Err(telegram::ApiError::RetryAfter(d)) => {
                        tokio::time::sleep(d).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        .instrument(tracing::info_span!("while inspecting GetUpdates query"))
    });

    let mut recorded_err: Option<eyre::Report> = None;

    let max_errs = 3;
    let mut err_count = 0;
    let err_handler = |e: S::Error| {
        // Got an error that can't be handled. Log an retry, until the
        // max number of errors.
        error!(e = ?e, "Telegram API error");
        err_count += 1;
        if err_count == max_errs {
            error!("Reached max number of retries");
            recorded_err = Some(e.into());
            false
        } else {
            true
        }
    };

    update_streamer_impl(wrapped, err_handler, sink).await;

    if let Some(e) = recorded_err {
        Err(e)
    } else {
        Ok(())
    }
}

async fn update_streamer_impl<S, H, E, Sink>(svc: S, mut err_handler: H, mut sink: Sink)
where
    S: Service<telegram::GetUpdates, Response = Vec<telegram::Update>, Error = E> + Clone,
    H: FnMut(E) -> bool,
    Sink: futures::Sink<telegram::Update> + Unpin,
{
    use futures::SinkExt;

    // Keep track of the number of consecutive failed requests. Retry until
    // max_errs.
    let max_errs = 3;
    let mut err_count = 0;

    let stream = util::stream_flatten_ok(UpdateBatchStream::new(svc));
    futures::pin_mut!(stream);

    async {
        loop {
            let u = stream.next().await.unwrap();

            match u {
                Ok(u) => {
                    err_count = 0;
                    if let Err(_) = sink.send(u).await {
                        break;
                    }
                }
                Err(e) => {
                    if !err_handler(e) {
                        break;
                    }
                }
            }
        }
    }
    .instrument(tracing::info_span!("processing update"))
    .await
}

#[pin_project::pin_project]
struct UpdateBatchStream<S: Service<telegram::GetUpdates>> {
    svc: S,
    next_offset: Option<i64>,
    #[pin]
    call_fut: Option<S::Future>,
}

impl<S: Service<telegram::GetUpdates>> UpdateBatchStream<S> {
    fn new(svc: S) -> Self {
        UpdateBatchStream {
            svc,
            next_offset: None,
            call_fut: None,
        }
    }

    fn do_call(
        req: telegram::GetUpdates,
        svc: &mut S,
        mut fut_place: std::pin::Pin<&mut Option<S::Future>>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<S::Response, S::Error>> {
        use std::task::ready;
        use std::task::Poll::*;

        if fut_place.as_ref().as_pin_ref().is_none() {
            // Must poll the service before attempting to call.
            match ready!(svc.poll_ready(cx)) {
                Ok(()) => (),
                Err(e) => return Ready(Err(e)),
            }

            fut_place.set(Some(svc.call(req)));
        }

        fut_place.as_pin_mut().unwrap().poll(cx)
    }
}

impl<S> futures::Stream for UpdateBatchStream<S>
where
    S: Service<telegram::GetUpdates, Response = Vec<telegram::Update>>,
{
    type Item = Result<Vec<telegram::Update>, S::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::ready;
        use std::task::Poll::*;

        let mut this = self.project();

        let batch = match ready!(Self::do_call(
            telegram::GetUpdates {
                offset: *this.next_offset,
                timeout: 30,
            },
            this.svc,
            this.call_fut.as_mut(),
            cx
        )) {
            Ok(b) => b,
            Err(e) => return Ready(Some(Err(e))),
        };

        // Compute the new offset. This should work even if the batch is
        // empty.
        *this.next_offset = batch
            .iter()
            .map(|u| u.update_id + 1)
            .chain(*this.next_offset)
            .max();

        Ready(Some(Ok(batch)))
    }
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");

#[cfg(test)]
mod test_util;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::*;

    use std::future::Future;

    use eyre::*;
    use futures::{FutureExt, SinkExt, TryFutureExt};
    use tokio::sync;
    use tokio::try_join;
    use tokio_util::sync::PollSender;
    use tower_test::*;
    use tracing::error_span;

    #[tokio::test]
    async fn test_update_batches() -> eyre::Result<()> {
        crate::setup_tracing();

        use telegram::{GetUpdates, Update};

        let (svc, mut controller) = mock::pair::<GetUpdates, Vec<Update>>();
        let svc = svc.map_err(|e| e.to_string());

        let mut batch_stream = UpdateBatchStream::new(svc);

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Vec<Update>, String>>(1);

        // Forward all `batch_stream` elements to the channel on a separate task.
        let stream_task = tokio::spawn(
            batch_stream
                .instrument(error_span!("in UpdateBatchStream"))
                .map(Ok)
                .forward(PollSender::new(sender).instrument(error_span!("sending to channel")))
                .instrument(error_span!("in stream task")),
        );

        fn try_batch<'a>(
            controller: &'a mut mock::Handle<GetUpdates, Vec<Update>>,
            receiver: &'a mut sync::mpsc::Receiver<Result<Vec<Update>, String>>,
            offset: Option<i64>,
            updates: Result<Vec<telegram::Update>, String>,
        ) -> impl Future<Output = eyre::Result<()>> + 'a {
            let updates2 = updates.clone();
            controller
                .next_request()
                .map(move |r| (r, offset))
                .map(|(r, offset)| match r {
                    Some((req, h)) => {
                        ensure!(
                            req.offset == offset,
                            "expected offset {:?}, got {:?}",
                            offset,
                            req.offset
                        );
                        match updates {
                            Ok(u) => h.send_response(u),
                            Err(e) => h.send_error(e),
                        }
                        Ok(())
                    }
                    None => Err(eyre!("no request")),
                })
                .and_then(|()| {
                    receiver
                        .recv()
                        .map(|r| r.ok_or_else(|| eyre!("sender dropped")))
                })
                .map(move |r| {
                    let received = r?;
                    ensure!(
                        matches!(received.as_deref(), updates2),
                        "received wrong update:\nexpected {updates2:?}\n\nreceived {received:?}"
                    );
                    Ok(())
                })
        }

        try_batch(
            &mut controller,
            &mut receiver,
            None,
            Ok(vec![telegram::Update {
                update_id: 2,
                message: None,
            }]),
        )
        .await?;

        std::assert_matches::assert_matches!(futures::poll!(stream_task), std::task::Poll::Pending);

        try_batch(
            &mut controller,
            &mut receiver,
            None,
            Ok(vec![telegram::Update {
                update_id: 2,
                message: None,
            }]),
        )
        .await?;

        Ok(())
    }
}
