//! E2E test: SDK connects to live chain, discovers compute, runs inference.
//!
//! Run with: cargo test -p arknet-e2e-sdk --test sdk_infer -- --nocapture

use std::time::Duration;

use arknet_sdk::session::SessionKey;
use arknet_sdk::wallet::Wallet;
use arknet_sdk::{Client, ConnectOptions, InferRequest};

#[tokio::main]
async fn main() {
    println!("=== arknet SDK e2e test ===\n");

    // 1. Create wallet
    let wallet = Wallet::create();
    println!("wallet address: 0x{}", wallet.address().to_hex());

    // 2. Create session key
    let session = SessionKey::create(&wallet, 1_000_000_000, Duration::from_secs(3600))
        .expect("session key creation failed");
    println!(
        "session key created, spending limit: {}, expires in 1h",
        session.remaining_limit()
    );

    // 3. Connect to mesh
    println!("\nconnecting to mesh via validator seed...");
    let client = Client::connect(ConnectOptions {
        seeds: vec![
            "/dns4/arknet.arkengel.com/tcp/26656/p2p/12D3KooWFKNZj7VaophcMVbA7QCRexAm7tg9dnADSJ8SxW4sLE1f"
                .into(),
        ],
        network_id: "mainnet".into(),
        discovery_timeout: Duration::from_secs(30),
        wallet: Some(wallet),
        session: Some(session),
    })
    .await
    .expect("failed to connect to mesh");

    println!(
        "connected! local peer: {}",
        client.candidates().len()
    );

    // 4. Check candidates
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let candidates = client.candidates().eligible_for("Qwen/Qwen3-0.6B-Q8_0", now_ms);
    println!("candidates for Qwen/Qwen3-0.6B-Q8_0: {}", candidates.len());

    if candidates.is_empty() {
        println!("no candidates found — gossip may not have propagated yet");
        println!("waiting 10 more seconds...");
        tokio::time::sleep(Duration::from_secs(10)).await;
        let candidates = client.candidates().eligible_for("Qwen/Qwen3-0.6B-Q8_0", now_ms + 10_000);
        println!("candidates after wait: {}", candidates.len());
        for c in &candidates {
            println!(
                "  peer: {}, models: {:?}, slots: {}, addrs: {:?}",
                hex::encode(&c.peer_id_bytes),
                c.model_refs,
                c.available_slots,
                c.multiaddrs
            );
        }
    } else {
        for c in &candidates {
            println!(
                "  peer: {}, models: {:?}, slots: {}, addrs: {:?}",
                hex::encode(&c.peer_id_bytes),
                c.model_refs,
                c.available_slots,
                c.multiaddrs
            );
        }
    }

    // 5. Try inference
    println!("\nsending inference request...");
    match client
        .infer(InferRequest {
            model: "Qwen/Qwen3-0.6B-Q8_0".into(),
            prompt: "Hello, what is arknet?".into(),
            max_tokens: 32,
            ..Default::default()
        })
        .await
    {
        Ok(resp) => {
            println!("inference response: {} bytes", resp.len());
            match borsh::from_slice::<Vec<arknet_compute::wire::InferenceJobEvent>>(&resp) {
                Ok(events) => {
                    for ev in &events {
                        match ev {
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
                        }
                    }
                }
                Err(e) => println!("failed to decode response: {e}"),
            }
        }
        Err(e) => println!("inference failed: {e}"),
    }

    client.shutdown();
    println!("\n=== done ===");
}
