//! Tests of the façade against a toy daemon (no fake needed).

use futures::stream;
use reno_proto::{ErrorCode, ErrorObject, ExecEvent};
use reno_rpc::{Router, Server, SubItem, connect_in_memory};
use tokio_util::sync::CancellationToken;

use super::*;

fn run() -> RunId {
    RunId::from_suffix("a").unwrap()
}

fn item(seq: u64) -> SubItem<RunEventItem> {
    SubItem::seq(
        Seq(seq),
        RunEventItem {
            seq: Seq(seq),
            event: ExecEvent::Text {
                text: seq.to_string(),
            },
        },
    )
}

/// A daemon whose `run.events` yields the given seqs and ends.
fn daemon_with_events(seqs: Vec<u64>) -> Router {
    Router::new()
        .method::<DaemonHello, _, _>(|_: HelloParams, _| async {
            Ok(HelloResult {
                server_version: "t".into(),
                protocol_version: PROTOCOL_VERSION,
                features: vec![],
                pid: 1,
            })
        })
        .subscription::<RunEvents, _, _, _>(move |_: RunEventsParams, _| {
            let seqs = seqs.clone();
            async move { Ok(stream::iter(seqs.into_iter().map(item))) }
        })
        .method::<RunGet, _, _>(|_: RunGetParams, _| async {
            Err::<RunSummary, _>(ErrorObject::new(ErrorCode::NotFound, "no run"))
        })
}

/// Connects to `router`. The server is returned so its tasks stay owned by the caller.
async fn connect(router: Router) -> (Client, Server, CancellationToken) {
    let cancel = CancellationToken::new();
    let server = Server::new(router);
    let rpc = connect_in_memory(&server, cancel.clone());
    (Client::connect(rpc).await.unwrap(), server, cancel)
}

#[tokio::test]
async fn a_hole_in_the_sequence_is_an_error_and_ends_the_stream() {
    let (client, _server, cancel) = connect(daemon_with_events(vec![1, 2, 4])).await;
    let mut events = Box::pin(client.run_events(run(), None));
    assert_eq!(events.next().await.unwrap().unwrap().seq, Seq(1));
    assert_eq!(events.next().await.unwrap().unwrap().seq, Seq(2));
    assert!(matches!(
        events.next().await,
        Some(Err(ClientError::Protocol(_)))
    ));
    assert!(events.next().await.is_none());
    cancel.cancel();
}

#[tokio::test]
async fn a_stream_that_ends_cleanly_is_complete() {
    let (client, _server, cancel) = connect(daemon_with_events(vec![2, 3, 4])).await;
    let seqs: Vec<_> = client
        .run_events(run(), Some(Seq(2)))
        .map(|r| r.unwrap().seq.0)
        .collect()
        .await;
    assert_eq!(seqs, [2, 3, 4]);
    cancel.cancel();
}

#[tokio::test]
async fn remote_errors_keep_their_code() {
    let (client, _server, cancel) = connect(daemon_with_events(vec![])).await;
    let err = client.get_run(&run()).await.unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotFound));
    cancel.cancel();
}

#[tokio::test]
async fn approvals_need_the_negotiated_feature() {
    // This daemon negotiated no features.
    let (client, _server, cancel) = connect(daemon_with_events(vec![])).await;
    assert!(!client.supports(reno_proto::FEATURE_APPROVALS));
    assert!(matches!(
        client.list_approvals().await,
        Err(ClientError::Protocol(_))
    ));
    cancel.cancel();
}

#[tokio::test]
async fn a_dropped_connection_ends_the_stream_with_disconnected() {
    let router = daemon_with_events(vec![]).subscription::<RunEvents, _, _, _>(
        |_: RunEventsParams, _| async { Ok(stream::pending::<SubItem<RunEventItem>>()) },
    );
    let (client, _server, cancel) = connect(router).await;
    let mut events = Box::pin(client.run_events(run(), None));
    let next = tokio::spawn(async move { events.next().await });
    tokio::task::yield_now().await;
    cancel.cancel();
    let got = next.await.unwrap();
    assert!(matches!(got, Some(Err(ClientError::Disconnected))));
}

#[tokio::test]
async fn connect_unix_shakes_hands_over_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    // `bind` creates the missing directory 0700 itself.
    let path = dir.path().join("sock/reno.sock");
    let listener = reno_rpc::UnixServer::bind(&path).await.unwrap();
    let server = Server::new(daemon_with_events(vec![]));
    let stop = CancellationToken::new();
    let serving = tokio::spawn({
        let stop = stop.clone();
        async move { server.serve(listener, stop).await }
    });

    let client = Client::connect_unix(&path).await.unwrap();
    assert_eq!(client.daemon().server_version, "t");
    drop(client);

    stop.cancel();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn connect_unix_reports_a_missing_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.sock");
    match Client::connect_unix(&path).await {
        Err(ClientError::NotRunning(p)) => assert_eq!(p, path),
        other => panic!("expected NotRunning, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_needs_the_negotiated_feature() {
    // The toy daemon negotiated no features.
    let (client, _server, cancel) = connect(daemon_with_events(vec![])).await;
    assert!(matches!(
        client.shutdown_daemon().await,
        Err(ClientError::Protocol(_))
    ));
    cancel.cancel();
}
