#![feature(assert_matches)]
#![feature(never_type)]

mod wolfram;

use tg_lib::http_service;
use tg_lib::telegram;

use futures::Future;
use telegram::Client;

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use eyre::WrapErr;
use tower::util::ServiceExt;
use tower::Service;
use tracing::{error, instrument};
use tracing_futures::Instrument;

use tokio::sync::mpsc;

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

    /// Given a telegram API error, decides whether to retry (after resolving the
    /// future) or quit and propagate the error.
    const MAX_API_ERRORS: u32 = 3;
    let err_count = Arc::new(AtomicU32::new(0));
    let handle_telegram_error = move |err: telegram::ApiError<_>| {
        let err_count = err_count.clone();
        async move {
            use telegram::ApiError::*;
            let wait_dur = match err {
                RetryAfter(dur) => dur,
                e => {
                    if err_count.fetch_add(1, Ordering::SeqCst) == MAX_API_ERRORS {
                        return Err(e);
                    }

                    std::time::Duration::from_secs(5)
                }
            };

            tokio::time::sleep(wait_dur).await;
            Ok(())
        }
    };

    let (sender, mut receiver) = mpsc::channel(100);
    let tg_update_task = tokio::task::spawn(update_task(tg.clone(), sender, handle_telegram_error));

    while let Some(u) = receiver.recv().await {
        let Some(telegram::UpdateDetail::Message(msg)) = u.detail else {continue};

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
    eyre::Report: From<E>,
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
    eyre::Report: From<E>,
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

/// Gets Telegram API updates by calling `svc` and forwards them on `chan`. When
/// the API returns an error, `inspect_err` is called. If `Ok(fut)`, the
/// returned future is awaited (e.g. to allow retrying after some time) then the
/// task continues. Otherwise, the task quits with the returned error.
async fn update_task<T, S, EFn, ContFut>(
    mut svc: S,
    chan: tokio::sync::mpsc::Sender<telegram::Update<T>>,
    mut inspect_err: EFn,
) -> eyre::Result<()>
where
    T: std::fmt::Debug,
    S: Service<telegram::GetUpdates, Response = Vec<telegram::Update<T>>>,
    S::Error: std::error::Error + Send + Sync + 'static,
    EFn: FnMut(S::Error) -> ContFut,
    ContFut: Future<Output = Result<(), S::Error>>,
{
    let timeout = 30;
    let mut cur_offset = None;

    loop {
        let batch = match (&mut svc)
            .oneshot(telegram::GetUpdates {
                offset: cur_offset,
                timeout,
            })
            .instrument(tracing::info_span!("getting next batch of updates"))
            .await
        {
            Ok(b) => b,
            Err(e) => {
                inspect_err(e).await?;
                continue;
            }
        };

        cur_offset = batch.iter().map(|u| u.update_id + 1).max().or(cur_offset);

        // Poll the channel in case the batch is empty, so we can close early.
        if chan.is_closed() {
            return Ok(());
        }

        for u in batch {
            tracing::info!("{cur_offset:?} {u:?}");
            if chan.send(u).await.is_err() {
                // Receiver was closed, so we may quit gracefully.
                return Ok(());
            }
        }
    }
}

static TELEGRAM_KEY: &str = include_str!("../.keys/telegram");
static WOLFRAM_KEY: &str = include_str!("../.keys/wolframalpha");

#[cfg(test)]
mod tests {
    use super::*;

    use test_util::*;

    use eyre::{ensure, eyre};

    use futures::FutureExt;
    use tokio::select;

    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    use tower_test::*;

    #[tokio::test]
    async fn update_batches() -> eyre::Result<()> {
        crate::setup_tracing();

        use telegram::{GetUpdates, Update};

        let (svc, mut controller) = mock::pair::<GetUpdates, Vec<Update<()>>>();
        let svc = svc.map_err(|e| EyreWrapper::from(eyre!(e)));

        let (send, mut recv) = mpsc::channel(100);
        let mut task_handle = tokio::spawn(update_task(svc, send, |e| async move { Err(e) }));

        async fn get_task_err(h: &mut JoinHandle<eyre::Result<()>>) -> eyre::Report {
            match h.await {
                Ok(Ok(())) => eyre!("task unexpectedly quit successfully"),
                Ok(Err(e)) => e.wrap_err("update task failed unexpectedly"),
                Err(e) => eyre!("task join error: {e:?}"),
            }
        }

        select! {
            biased;
            e = get_task_err(&mut task_handle) => Err(e),
            Some((req, handle)) = controller.next_request() => {
                eyre::ensure!(req.offset.is_none(), "{:?}", req.offset);
                handle.send_response(vec![telegram::Update {
                    update_id: 123,
                    detail: None,
                }]);

                let resp = recv.recv().then(or_pending).await;
                ensure!(matches!(resp, telegram::Update { update_id: 123, detail: None}), "incorrect response");

                Ok(())
            },
            else => unreachable!(),
        }?;

        select! {
            biased;
            e = get_task_err(&mut task_handle) => Err(e),
            Some((req, handle)) = controller.next_request() => {
                eyre::ensure!(req.offset == Some(124), "{:?}", req.offset);
                handle.send_response(vec![telegram::Update {
                    update_id: 125,
                    detail: Some(()),
                }]);

                let resp = recv.recv().then(or_pending).await;
                ensure!(matches!(resp, telegram::Update { update_id: 125, detail: Some(())}), "incorrect response: {resp:?}");

                Ok(())
            },
            else => unreachable!(),
        }?;

        select! {
            biased;
            e = get_task_err(&mut task_handle) => Err(e),
            Some((req, handle)) = controller.next_request() => {
                eyre::ensure!(req.offset == Some(126), "{:?}", req.offset);
                handle.send_response(vec![telegram::Update {
                    update_id: 127,
                    detail: Some(()),
                }, telegram::Update {
                    update_id: 128,
                    detail: None,
                }]);

                let resp = recv.recv().then(or_pending).await;
                ensure!(matches!(resp, telegram::Update { update_id: 127, detail: Some(())}), "incorrect response: {resp:?}");

                let resp = recv.recv().then(or_pending).await;
                ensure!(matches!(resp, telegram::Update { update_id: 128, detail: None}), "incorrect response: {resp:?}");

                Ok(())
            },
            else => unreachable!(),
        }?;

        let (req, handle) = controller.next_request().await.unwrap();
        eyre::ensure!(req.offset == Some(129), "{:?}", req.offset);
        handle.send_error("foo bar");
        // Our error test handler fn should cause the task to quit
        // immediately.
        let err_string = task_handle
            .await?
            .err()
            .ok_or_else(|| eyre!("task was successful?"))?
            .to_string();
        ensure!(err_string == "foo bar", "{err_string}");

        Ok(())
    }

    #[tokio::test]
    async fn update_task_quits_gracefully() -> eyre::Result<()> {
        crate::setup_tracing();

        use telegram::{GetUpdates, Update};

        let (svc, mut controller) = mock::pair::<GetUpdates, Vec<Update<()>>>();
        let svc = svc.map_err(|_| EyreWrapper::from(eyre!("")));

        let (send, mut recv) = mpsc::channel(100);
        let task_handle = tokio::spawn(update_task(svc, send, |e| async move { Err(e) }));

        let (_, h) = controller.next_request().await.unwrap();
        h.send_response(vec![]);

        let (_, h) = controller.next_request().await.unwrap();
        h.send_response(vec![]);

        let (_, h) = controller.next_request().await.unwrap();
        recv.close();
        h.send_response(vec![]);

        task_handle.await?
    }

    #[tokio::test]
    async fn update_task_obeys_err_fn() -> eyre::Result<()> {
        crate::setup_tracing();

        use std::sync::Arc;

        use telegram::{GetUpdates, Update};

        let (svc, mut controller) = mock::pair::<GetUpdates, Vec<Update<()>>>();
        let svc = svc.map_err(|_| EyreWrapper::from(eyre!("")));

        // Send/recv a bool to decide whether to quit in `handle_err`. One
        // message per API error. Since the channel has length 1, each send
        // will wait until the error is handled.
        let (quit_send, quit_recv) = mpsc::channel(1);

        let quit_recv = Arc::new(Mutex::new(quit_recv));
        let handle_err = move |e| {
            let quit_recv = quit_recv.clone();
            async move {
                let should_quit = quit_recv.lock().await.recv().await.unwrap();
                if should_quit {
                    Err(e)
                } else {
                    Ok(())
                }
            }
        };

        let (send, _recv) = mpsc::channel(100);
        let task_handle = tokio::spawn(update_task(svc, send, handle_err));

        let (_, h) = controller.next_request().await.unwrap();
        h.send_response(vec![]);

        let (_, h) = controller.next_request().await.unwrap();
        h.send_error(eyre!(""));
        quit_send.send(false).await?;

        let (_, h) = controller.next_request().await.unwrap();
        h.send_error(eyre!(""));
        quit_send.send(false).await?;

        let (_, h) = controller.next_request().await.unwrap();
        h.send_response(vec![]);

        let (_, h) = controller.next_request().await.unwrap();
        h.send_error(eyre!(""));
        quit_send.send(true).await?;

        eyre::ensure!(
            controller.next_request().await.is_none(),
            "task did not quit"
        );
        let err = task_handle
            .await?
            .err()
            .ok_or(eyre!("task did not return the error"))?;
        eyre::ensure!(err.to_string() == "", "task returned wrong error?");

        Ok(())
    }
}
