//! Reno's Pi gate extension (`src/drivers/pi/gate.mjs`) against a scripted Unix socket, under
//! Node. Pi itself is not involved: the gate is an extension function, driven here with the calls
//! Pi makes of it (`on`, `registerCommand`). Node must be installed (Pi is a Node program, so
//! anywhere Pi runs it is); without it the test fails, since a gate nothing ran is not checked.

#[tokio::test]
async fn the_gate_sends_every_call_and_blocks_whatever_it_cannot_judge() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut node = tokio::process::Command::new("node");
    node.arg(root.join("tests/fixtures/pi-gate-test.mjs"))
        .arg(root.join("src/drivers/pi/gate.mjs"))
        .kill_on_drop(true);
    let output = match tokio::time::timeout(std::time::Duration::from_secs(60), node.output()).await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            panic!("node is not installed: the Pi gate test cannot run")
        }
        Ok(Err(error)) => panic!("node did not run: {error}"),
        Err(_) => panic!("the Pi gate test must finish without waiting on a clock"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
