//! `task.run` attachments (T1.11a): the daemon checks each file and appends an "Attached files:"
//! list of their paths to the prompt, so a driver needs no change. No network, no real agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, id, within};
use reno_proto::{
    ATTACHMENT_MAX_BYTES, ATTACHMENTS_MAX_COUNT, Attachment, ErrorCode, FEATURES, RunStatus,
    TaskRunParams,
};

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

fn task(attachments: Vec<Attachment>) -> TaskRunParams {
    TaskRunParams {
        prompt: "read these".into(),
        executor: Some(id("mock")),
        attachments,
        ..TaskRunParams::default()
    }
}

fn attach(path: &std::path::Path, mime: Option<&str>) -> Attachment {
    Attachment {
        path: path.to_path_buf(),
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        mime: mime.map(str::to_owned),
    }
}

async fn refused(fx: &Fixture, attachments: Vec<Attachment>) -> String {
    let error = within(fx.client.run_task(task(attachments)))
        .await
        .expect_err("the daemon refuses it");
    assert_eq!(error.code(), Some(ErrorCode::InvalidParams));
    error.to_string()
}

daemon_test! {
    async fn attached_files_are_listed_at_the_end_of_the_prompt() {
        assert!(FEATURES.contains(&"attachments"));
        let fx = Fixture::start().await;
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&shot, b"png").unwrap();
        std::fs::write(&notes, b"text").unwrap();
        // A symlink is listed by what it points to.
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&notes, &link).unwrap();

        let run = within(fx.client.run_task(task(vec![
            attach(&shot, Some("image/png")),
            attach(&link, None),
        ])))
        .await
        .unwrap();
        fx.events(&run, None).await;
        assert_eq!(fx.status(&run).await, RunStatus::Succeeded);

        let shot = std::fs::canonicalize(&shot).unwrap();
        let notes = std::fs::canonicalize(&notes).unwrap();
        let prompt = within(fx.client.get_run(&run)).await.unwrap().prompt.unwrap();
        assert_eq!(
            prompt,
            format!(
                "read these\n\nAttached files:\n- {} (shot.png, image/png)\n- {} (link.txt)\n",
                shot.display(),
                notes.display()
            )
        );
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_task_without_attachments_keeps_its_prompt() {
        let fx = Fixture::start().await;
        let run = within(fx.client.run_task(task(vec![]))).await.unwrap();
        fx.events(&run, None).await;
        let prompt = within(fx.client.get_run(&run)).await.unwrap().prompt.unwrap();
        assert_eq!(prompt, "read these");
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_bad_attachment_refuses_the_task_and_creates_no_run() {
        let fx = Fixture::start().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let missing = dir.path().join("missing.txt");
        assert!(refused(&fx, vec![attach(&missing, None)]).await.contains("cannot be read"));

        let relative = Attachment { path: "a.txt".into(), name: "a.txt".into(), mime: None };
        assert!(refused(&fx, vec![relative]).await.contains("not an absolute path"));

        assert!(refused(&fx, vec![attach(dir.path(), None)]).await.contains("not a file"));

        let many = vec![attach(&file, None); ATTACHMENTS_MAX_COUNT + 1];
        assert!(refused(&fx, many).await.contains("too many attachments"));

        // Not readable: only meaningful for a user that mode bits bind.
        let locked = dir.path().join("locked.txt");
        std::fs::write(&locked, b"x").unwrap();
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        if std::fs::File::open(&locked).is_err() {
            assert!(refused(&fx, vec![attach(&locked, None)]).await.contains("cannot be read"));
        }

        assert!(within(fx.client.list_runs(Default::default())).await.unwrap().is_empty());
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_file_over_the_size_cap_is_refused() {
        let fx = Fixture::start().await;
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(ATTACHMENT_MAX_BYTES + 1).unwrap();
        assert!(refused(&fx, vec![attach(&big, None)]).await.contains("at most 25 MiB"));
        fx.stop().await;
    }
}
