use serde_json::json;
use std::time::Duration;
use tokio::{process::Command, time::timeout};
use trefoil_backend::mcp_stdio::{LocalMcpTransport, TransportError};

fn fixture(mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-stdio-fixture"));
    command.arg(mode);
    command
}

#[tokio::test]
async fn request_and_notification_preserve_stdio_wire_contract() {
    let transport = LocalMcpTransport::spawn(fixture("success")).await.unwrap();
    transport.notify("notify", None).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), transport.next_notification())
            .await
            .unwrap()
            .unwrap(),
        json!({"jsonrpc":"2.0","method":"notice","params":{"seen":true}})
    );
    assert_eq!(
        transport.request("request", "call", None).await.unwrap(),
        json!({"ok":true})
    );
    transport.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_eof_completes_pending_request_and_cleanup_is_repeatable() {
    let transport = LocalMcpTransport::spawn(fixture("eof")).await.unwrap();
    let result = timeout(
        Duration::from_secs(1),
        transport.request("request", "call", None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(result, TransportError::Closed));
    assert!(transport.pid().await.is_some());
    transport.shutdown().await.unwrap();
    transport.shutdown().await.unwrap();
    assert!(transport.pid().await.is_none());
}

#[tokio::test]
async fn malformed_child_output_is_reported_to_pending_request() {
    let transport = LocalMcpTransport::spawn(fixture("malformed"))
        .await
        .unwrap();
    let result = timeout(
        Duration::from_secs(1),
        transport.request("request", "call", None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(result, TransportError::Json(_)));
    transport.shutdown().await.unwrap();
}
