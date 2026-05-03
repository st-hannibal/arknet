//! E2E test: SDK connects to live chain, discovers compute, runs inference.

use std::time::Duration;

use arknet_sdk::session::SessionKey;
use arknet_sdk::wallet::Wallet;
use arknet_sdk::{Client, ConnectOptions, InferRequest};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,arknet_network=debug,arknet_sdk=debug"
                    .parse()
                    .unwrap()
            }),
        )
        .init();

    println!("=== arknet SDK e2e test ===\n");

    let wallet = Wallet::create();
    println!("wallet address: 0x{}", wallet.address().to_hex());

    let session = SessionKey::create(&wallet, 1_000_000_000, Duration::from_secs(3600))
        .expect("session key creation failed");
    println!("session key created, expires in 1h\n");

    println!("connecting to mesh via seed...");
    let client = Client::connect(ConnectOptions {
        seeds: vec![],
        network_id: "mainnet".into(),
        discovery_timeout: Duration::from_secs(60),
        wallet: Some(wallet),
        session: Some(session),
    })
    .await
    .expect("failed to connect to mesh");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let candidates = client
        .candidates()
        .eligible_for("Qwen/Qwen3-0.6B-Q8_0", now_ms);
    println!("candidates: {}", candidates.len());
    for c in &candidates {
        println!(
            "  peer: {}, models: {:?}, slots: {}",
            hex::encode(&c.peer_id_bytes),
            c.model_refs,
            c.available_slots,
        );
    }

    println!("\nsending inference request via mesh...");
    match client
        .infer(InferRequest {
            model: "Qwen/Qwen3-0.6B-Q8_0".into(),
            prompt: "Hi, how are you?".into(),
            max_tokens: 64,
            ..Default::default()
        })
        .await
    {
        Ok(resp) => {
            println!("response: {} bytes", resp.len());
            let ir: arknet_network::InferenceResponse = match borsh::from_slice(&resp) {
                Ok(r) => r,
                Err(e) => {
                    println!("decode outer: {e}");
                    return;
                }
            };
            for raw in &ir.events {
                match borsh::from_slice::<arknet_compute::wire::InferenceJobEvent>(raw) {
                    Ok(ev) => match ev {
                        arknet_compute::wire::InferenceJobEvent::Token { text, .. } => {
                            print!("{text}");
                        }
                        arknet_compute::wire::InferenceJobEvent::Stop { reason, .. } => {
                            println!("\n[stop: {reason:?}]");
                        }
                        arknet_compute::wire::InferenceJobEvent::Error { message, .. } => {
                            println!("\n[error: {message}]");
                        }
                        arknet_compute::wire::InferenceJobEvent::Busy { .. } => {
                            println!("\n[busy]");
                        }
                    },
                    Err(e) => println!("[decode event: {e}]"),
                }
            }
        }
        Err(e) => println!("inference failed: {e}"),
    }

    client.shutdown();
    println!("\n=== done ===");
}
