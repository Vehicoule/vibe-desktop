use futures::StreamExt;
use std::path::PathBuf;
use vibe_protocol::client::Connection;
use vibe_protocol::models::*;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let fx = PathBuf::from(
        std::env::var("CARGO_BIN_EXE_vibe-fixture")
            .unwrap_or_else(|_| "target/debug/vibe-fixture".to_string()),
    );
    let mut conn = Connection::spawn(&fx).await.expect("spawn");
    conn.initialize(
        ClientInfo {
            name: "t".into(),
            version: "0".into(),
            title: None,
            entrypoint: "t".into(),
            terminal_emulator: "t".into(),
        },
        ClientCapabilities {
            callback_kinds: vec![],
            client_tools: vec![],
            disabled_notifications: vec![],
        },
    )
    .await
    .unwrap();
    let st = conn
        .session_start(SessionStartParams {
            agent_config: AgentConfig::default(),
            history_limit: 50,
            idempotency_key: None,
            kind: None,
        })
        .await
        .unwrap();
    println!("session {}", st.session.id);
    conn.turn_start(&st.session.id, "hi").await.unwrap();
    let mut rx = conn.take_events().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        let m = match tokio::time::timeout(std::time::Duration::from_secs(3), rx.next()).await {
            Ok(Some(m)) => m,
            _ => {
                println!("TIMEOUT/EOS");
                break;
            }
        };
        match &m {
            ServerMessage::Notification { method, .. } => println!("notif {method}"),
            ServerMessage::Request { id, method, params } => {
                println!("REQ {method}");
                if method == "callback/call" {
                    let cb = params["callback"]["callbackId"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    conn.respond(id, serde_json::json!({"callbackId": cb, "accepted": true}))
                        .await
                        .unwrap();
                    let r = conn
                        .callback_respond(
                            &st.session.id,
                            &cb,
                            CallbackOutput::Approval {
                                decision: ApprovalDecision::of("approve"),
                                feedback: None,
                            },
                        )
                        .await;
                    println!("respond -> {r:?}");
                }
            }
        }
        if matches!(&m, ServerMessage::Notification { method, .. } if method == "turn/completed") {
            println!("DONE");
            break;
        }
    }
}
