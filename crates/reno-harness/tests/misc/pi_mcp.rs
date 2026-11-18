//! Reno's Pi MCP bridge (`src/drivers/pi/mcp.mjs`) against a real stdio MCP peer, under Node.
//! Pi itself is not involved: the bridge is an extension function, driven here with the two
//! calls Pi makes of it (`on`, `registerTool`). Node must be installed (Pi is a Node program, so
//! anywhere Pi runs it is); without it the test says so and passes, since there is nothing to run.

#[tokio::test]
async fn the_bridge_discovers_names_calls_cancels_and_reaps() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut node = tokio::process::Command::new("node");
    node.arg(root.join("tests/fixtures/pi-mcp-test.mjs"))
        .arg(root.join("src/drivers/pi/mcp.mjs"))
        .kill_on_drop(true);
    let output = match tokio::time::timeout(std::time::Duration::from_secs(30), node.output()).await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("node is not installed: the Pi MCP bridge test did not run");
            return;
        }
        Ok(Err(error)) => panic!("node did not run: {error}"),
        Err(_) => panic!("the Pi MCP bridge must close all its subprocesses"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
