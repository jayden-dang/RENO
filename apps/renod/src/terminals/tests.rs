#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The Terminal service over the fake PTY: no test here starts a shell.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use reno_proto::{TERM_MAX_TERMINALS, TERM_REPLAY_BYTES, TermEvent, encode_bytes};

use super::fake::Op;
use super::*;

/// A failsafe, not a measurement.
const FAILSAFE: Duration = Duration::from_secs(20);

async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(FAILSAFE, fut)
        .await
        .expect("timed out: the terminal stopped making progress")
}

fn fast() -> Timing {
    Timing {
        batch: Duration::from_millis(1),
        kill_grace: Duration::from_millis(50),
        eof_grace: Duration::from_millis(50),
        exited_ttl: Duration::from_secs(TERM_EXITED_TTL_SECS),
    }
}

fn service(pty: &FakePty) -> Terminals {
    Terminals::new(
        Some(Arc::new(pty.clone())),
        vec![
            ("HOME".into(), "/home/p".into()),
            ("RENO_RUN_TOKEN".into(), "rnt_secret".into()),
            ("SHELL".into(), "/bin/sh".into()),
        ],
        fast(),
    )
}

fn request(dir: &Path, live: bool) -> OpenRequest {
    OpenRequest {
        dir: dir.to_path_buf(),
        in_place: false,
        live,
        cols: 80,
        rows: 24,
        command: None,
    }
}

/// The bytes of the next `n` data events, and the events.
async fn take(
    stream: &mut (impl futures::Stream<Item = TermEvent> + Unpin),
    n: usize,
) -> Vec<TermEvent> {
    let mut got = Vec::new();
    while got.len() < n {
        got.push(within(stream.next()).await.expect("the stream ended"));
    }
    got
}

fn text(events: &[TermEvent]) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(TermEvent::bytes)
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_starts_in_the_tree_as_a_login_shell_with_its_own_environment() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    assert!(!opened.sandboxed && !opened.in_place);
    assert_eq!(opened.shell, "sh");
    let spec = pty.processes()[0].spec();
    assert_eq!(spec.program, Path::new("/bin/sh"));
    assert_eq!(spec.args, vec![OsString::from("-l")]);
    assert_eq!(spec.cwd, dir.path().canonicalize().unwrap());
    assert_eq!((spec.cols, spec.rows), (80, 24));
    assert!(spec.limits.is_none());
    let names: Vec<_> = spec.env.iter().map(|(n, _)| n.to_str().unwrap()).collect();
    assert!(names.contains(&"HOME") && names.contains(&"TERM_PROGRAM"));
    assert!(!names.iter().any(|n| n.starts_with("RENO_")), "{names:?}");
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_a_live_run_is_limited_to_the_real_path_and_survives_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("via-link");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(&link, true)).await.unwrap();
    assert!(opened.sandboxed);
    let canonical = real.canonicalize().unwrap();
    assert_eq!(opened.cwd, canonical.display().to_string());
    let limits = pty.processes()[0].spec().limits.expect("limits");
    assert_eq!(limits.root, canonical);
    // It is listed, and keeps being sandboxed whatever happens to the run.
    let listed = terms.list(&link);
    assert_eq!(listed.len(), 1);
    assert!(listed[0].sandboxed);
    assert!(terms.status(&opened.id).unwrap().sandboxed);
    // A terminal in a folder that is the person's own is not limited, live or not.
    let mut in_place = request(dir.path(), true);
    in_place.in_place = true;
    let free = terms.open(in_place).await.unwrap();
    assert!(!free.sandboxed && free.in_place);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_sandbox_a_live_run_gets_no_terminal_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    pty.refuse_sandbox(Some("sandbox-exec is missing"));
    let terms = service(&pty);
    let error = terms.open(request(dir.path(), true)).await.unwrap_err();
    assert!(matches!(error, TermError::SandboxUnavailable(_)), "{error}");
    assert_eq!(pty.started(), 0);
    assert_eq!(terms.count(), 0);
    // A run that is not live needs no sandbox.
    terms.open(request(dir.path(), false)).await.unwrap();
    assert_eq!(pty.started(), 1);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_echoed_as_numbered_output_and_exit_ends_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::echo();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let mut stream = Box::pin(terms.attach(&opened.id, None).unwrap());
    terms.write(&opened.id, &encode_bytes(b"ls\r")).unwrap();
    let got = take(&mut stream, 1).await;
    assert_eq!(text(&got), "ls\r");
    assert_eq!(got[0].seq(), 1);
    terms.write(&opened.id, &encode_bytes(b"exit\r")).unwrap();
    let mut rest = Vec::new();
    while let Some(event) = within(stream.next()).await {
        rest.push(event);
    }
    let last = rest.last().expect("an exit");
    assert!(
        matches!(last, TermEvent::Exit { code: Some(0), .. }),
        "{rest:?}"
    );
    assert!(terms.status(&opened.id).unwrap().exited);
    // Writing to it now says so.
    let error = terms.write(&opened.id, &encode_bytes(b"x")).unwrap_err();
    assert!(matches!(error, TermError::Exited(_)), "{error}");
    // Attaching again replays through the exit.
    let mut again = Box::pin(terms.attach(&opened.id, Some(0)).unwrap());
    let mut all = Vec::new();
    while let Some(event) = within(again.next()).await {
        all.push(event);
    }
    assert_eq!(text(&all), "ls\rexit\r");
    assert!(matches!(all.last(), Some(TermEvent::Exit { .. })));
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_that_comes_back_replays_from_its_last_seq() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let shell = pty.processes()[0].clone();
    let mut first = Box::pin(terms.attach(&opened.id, None).unwrap());
    shell.emit(b"one ");
    let a = take(&mut first, 1).await;
    drop(first);
    // Output while detached.
    shell.emit(b"two ");
    shell.emit(b"three ");
    let mut second = Box::pin(terms.attach(&opened.id, Some(a[0].seq())).unwrap());
    let mut seen = String::new();
    let mut last = a[0].seq();
    while !seen.contains("three") {
        let event = take(&mut second, 1).await.remove(0);
        assert!(event.seq() > last, "{event:?} after {last}");
        last = event.seq();
        seen.push_str(&text(std::slice::from_ref(&event)));
    }
    assert_eq!(seen.replace(' ', ""), "twothree");
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_older_than_the_window_is_told_what_it_lost() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let shell = pty.processes()[0].clone();
    // Three times the window, in chunks, then a marker.
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..(3 * TERM_REPLAY_BYTES / chunk.len()) {
        shell.emit(&chunk);
    }
    shell.emit(b"END");
    // Wait until the marker is in the ring.
    let mut probe = Box::pin(terms.attach(&opened.id, Some(u64::MAX)).unwrap());
    let mut tail = String::new();
    while !tail.ends_with("END") {
        let event = take(&mut probe, 1).await.remove(0);
        tail.push_str(&text(&[event]));
    }
    drop(probe);

    let mut stream = Box::pin(terms.attach(&opened.id, Some(0)).unwrap());
    let first = take(&mut stream, 2).await;
    let TermEvent::Gap { lost_from, seq } = first[0] else {
        panic!("a gap first: {first:?}");
    };
    assert_eq!(lost_from, 1);
    assert!(seq > 1);
    assert_eq!(
        first[1].seq(),
        seq,
        "the item after the gap carries its seq"
    );
    // Replay holds no more than the window, and ends with the marker.
    let mut bytes = first[1].bytes().unwrap().len();
    let mut last_text = String::new();
    while !last_text.ends_with("END") {
        let event = take(&mut stream, 1).await.remove(0);
        bytes += event.bytes().map_or(0, |b| b.len());
        last_text = text(&[event]);
    }
    assert!(bytes <= TERM_REPLAY_BYTES + 64 * 1024, "{bytes}");
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_and_resizes_reach_the_shell_in_the_order_they_were_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let shell = pty.processes()[0].clone();
    let mut want = Vec::new();
    // Fifteen pairs fit the lane's queue; a viewer keeps far fewer in flight.
    for n in 0..15_u16 {
        terms
            .write(&opened.id, &encode_bytes(format!("w{n};").as_bytes()))
            .unwrap();
        want.push(Op::Write(format!("w{n};").into_bytes()));
        terms.resize(&opened.id, 100 + n, 30).unwrap();
        want.push(Op::Resize(100 + n, 30));
    }
    within(async {
        while shell.ops().len() < want.len() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(shell.ops(), want);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_that_does_not_read_backs_its_own_lane_up_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let stuck = terms.open(request(dir.path(), false)).await.unwrap();
    let fine = terms.open(request(dir.path(), false)).await.unwrap();
    pty.processes()[0].stall(true);
    let mut refused = None;
    for _ in 0..200 {
        if let Err(error) = terms.write(&stuck.id, &encode_bytes(b"x")) {
            refused = Some(error);
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        matches!(refused, Some(TermError::Backlog(_))),
        "{refused:?}"
    );
    // The other terminal is unaffected.
    terms.write(&fine.id, &encode_bytes(b"ok")).unwrap();
    let other = pty.processes()[1].clone();
    within(async {
        while other.written().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    pty.processes()[0].stall(false);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_checked_before_it_is_queued() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    assert!(matches!(
        terms.write(&opened.id, "not base64 !!").unwrap_err(),
        TermError::BadInput(_)
    ));
    let big = encode_bytes(&vec![b'a'; TERM_MAX_INPUT_BYTES + 1]);
    assert!(matches!(
        terms.write(&opened.id, &big).unwrap_err(),
        TermError::BadInput(_)
    ));
    let missing = TermId::from_suffix("nope").unwrap();
    assert!(matches!(
        terms.write(&missing, "").unwrap_err(),
        TermError::NotFound(_)
    ));
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_says_busy_while_a_program_holds_the_foreground() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let shell = pty.processes()[0].clone();
    assert!(!terms.status(&opened.id).unwrap().busy);
    shell.set_busy(true);
    assert!(terms.status(&opened.id).unwrap().busy);
    shell.set_busy(false);
    shell.exit(3);
    within(async {
        while !terms.status(&opened.id).unwrap().exited {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(!terms.status(&opened.id).unwrap().busy);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_holds_sixteen_terminals_and_makes_room_with_an_exited_one() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let mut ids = Vec::new();
    for _ in 0..TERM_MAX_TERMINALS {
        ids.push(terms.open(request(dir.path(), false)).await.unwrap().id);
    }
    let error = terms.open(request(dir.path(), false)).await.unwrap_err();
    assert!(matches!(error, TermError::TooMany(16)), "{error}");
    // One shell exits: its slot is taken for the new terminal.
    pty.processes()[4].exit(0);
    within(async {
        while !terms.status(&ids[4]).unwrap().exited {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let fresh = terms.open(request(dir.path(), false)).await.unwrap();
    assert_eq!(terms.count(), TERM_MAX_TERMINALS);
    assert!(matches!(
        terms.status(&ids[4]).unwrap_err(),
        TermError::NotFound(_)
    ));
    assert!(terms.status(&fresh.id).is_ok());
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_hangs_up_then_kills_a_shell_that_ignores_it() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let polite = terms.open(request(dir.path(), false)).await.unwrap();
    let stubborn = terms.open(request(dir.path(), false)).await.unwrap();
    pty.processes()[1].ignore_hangup(true);
    let mut watching = Box::pin(terms.attach(&stubborn.id, None).unwrap());
    terms.close(&polite.id).unwrap();
    terms.close(&stubborn.id).unwrap();
    assert!(pty.processes()[0].hung_up() && !pty.processes()[0].killed());
    assert!(matches!(
        terms.status(&polite.id).unwrap_err(),
        TermError::NotFound(_)
    ));
    // The stubborn one is killed once the grace passes, and a viewer sees it end.
    let mut last = None;
    while let Some(event) = within(watching.next()).await {
        last = Some(event);
    }
    assert!(matches!(last, Some(TermEvent::Exit { .. })), "{last:?}");
    assert!(pty.processes()[1].killed());
    assert_eq!(terms.count(), 0);
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_tree_closes_its_terminals_and_names_them() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let in_a1 = terms.open(request(&a, false)).await.unwrap().id;
    let in_a2 = terms.open(request(&a, false)).await.unwrap().id;
    let in_b = terms.open(request(&b, false)).await.unwrap().id;
    assert_eq!(terms.list(&a).len(), 2);
    let mut closed = terms.close_dir(&a);
    closed.sort();
    let mut want = vec![in_a1, in_a2];
    want.sort();
    assert_eq!(closed, want);
    assert!(terms.list(&a).is_empty());
    assert_eq!(terms.list(&b)[0].id, in_b);
    assert!(terms.close_dir(&a).is_empty());
    terms.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_exited_terminal_is_reaped_after_thirty_minutes_by_a_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let mut stream = Box::pin(terms.attach(&opened.id, None).unwrap());
    pty.processes()[0].exit(0);
    // The exit is delivered by real threads; the paused clock waits for them.
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    assert!(matches!(last, Some(TermEvent::Exit { .. })));
    assert_eq!(terms.list(dir.path()).len(), 1);
    tokio::time::advance(Duration::from_secs(29 * 60)).await;
    tokio::task::yield_now().await;
    assert_eq!(terms.list(dir.path()).len(), 1, "not yet");
    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(terms.list(dir.path()).is_empty(), "reaped");
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_runs_from_a_private_script_inside_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let mut ask = request(dir.path(), false);
    ask.command = Some("npm test\necho done".into());
    let opened = terms.open(ask).await.unwrap();
    let shell = pty.processes()[0].clone();
    let script = shell
        .spec()
        .env
        .iter()
        .find(|(n, _)| n == "TERM_ACTION_SCRIPT")
        .map(|(_, v)| std::path::PathBuf::from(v))
        .expect("the script's path is in the shell's environment");
    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        "npm test\necho done\n"
    );
    within(async {
        while shell.written().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(shell.written(), b". \"$TERM_ACTION_SCRIPT\"\r");
    // The script goes when the shell does.
    shell.exit(0);
    within(async {
        while !terms.status(&opened.id).unwrap().exited {
            tokio::task::yield_now().await;
        }
    })
    .await;
    within(async {
        while script.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_a_spawner_answers_disabled_and_a_gone_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let off = Terminals::new(None, Vec::new(), fast());
    assert!(!off.enabled());
    assert!(matches!(
        off.open(request(dir.path(), false)).await.unwrap_err(),
        TermError::Disabled
    ));
    let pty = FakePty::silent();
    let terms = service(&pty);
    let gone = dir.path().join("gone");
    assert!(matches!(
        terms.open(request(&gone, false)).await.unwrap_err(),
        TermError::FolderUnavailable(_)
    ));
    assert_eq!(pty.started(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_every_shell_and_every_task() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    let opened = terms.open(request(dir.path(), false)).await.unwrap();
    let mut stream = Box::pin(terms.attach(&opened.id, None).unwrap());
    within(terms.shutdown()).await;
    assert!(pty.processes()[0].hung_up());
    assert_eq!(terms.count(), 0);
    // A viewer attached at the time is not left hanging.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends or delivers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_shell_is_started_into_a_daemon_that_is_stopping() {
    let dir = tempfile::tempdir().unwrap();
    let pty = FakePty::silent();
    let terms = service(&pty);
    terms.shutdown().await;
    let error = terms.open(request(dir.path(), false)).await.unwrap_err();
    assert!(matches!(error, TermError::ShuttingDown), "{error}");
    assert_eq!(pty.started(), 0);
}

#[test]
fn a_terminals_threads_are_its_own_and_leave_the_blocking_pool_alone() {
    // The daemon's blocking pool is a few threads, for the store. Terminals park three threads
    // each for as long as their shells live; a pool of two would be gone after one terminal if
    // they used it, and a task that needs the pool must still run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let pty = FakePty::silent();
        let terms = service(&pty);
        for _ in 0..6 {
            terms.open(request(dir.path(), false)).await.unwrap();
        }
        pty.processes()[0].stall(true);
        let id = terms.list(dir.path())[0].id.clone();
        terms.write(&id, &encode_bytes(b"stuck")).unwrap();
        let ran = within(tokio::task::spawn_blocking(|| 7)).await.unwrap();
        assert_eq!(ran, 7);
        pty.processes()[0].stall(false);
        terms.shutdown().await;
    });
}
