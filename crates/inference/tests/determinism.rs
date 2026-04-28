//! End-to-end determinism integration test.
//!
//! The hard gate for Phase 0 Week 9-10: the same prompt, run twice
//! through `InferenceMode::Deterministic`, must produce byte-identical
//! token streams.
//!
//! Uses the `stories260K` fixture — a 260K-parameter TinyLlama trained
//! on TinyStories, pinned at SHA-256 in the test setup. Fixture is
//! fetched on demand and cached under `<tempdir>/arknet-test-fixtures/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use arknet_crypto::hash::sha256;
use arknet_inference::{
    Context, ContextParams, EventSink, InferenceEvent, InferenceMode, Model, ModelLoadParams,
    Sampler, SamplingParams, Session, SessionRequest, StopReason, Tokenizer,
};

const STORIES260K_URL: &str =
    "https://huggingface.co/ggml-org/models/resolve/main/tinyllamas/stories260K.gguf";
const STORIES260K_SHA256: &str = "270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d";

fn fixture_dir() -> PathBuf {
    std::env::var("ARKNET_TEST_FIXTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("arknet-test-fixtures"))
}

fn fixture_path() -> PathBuf {
    if let Ok(p) = std::env::var("ARKNET_TEST_STORIES260K") {
        return PathBuf::from(p);
    }
    fixture_dir().join("stories260K.gguf")
}

/// Return the fixture path, fetching it if necessary. `None` if the
/// network fetch fails and no cached copy exists — caller skips.
///
/// This binary's three tests run in parallel by default; without the
/// mutex they all hit the `curl -o` path simultaneously on a cold
/// runner and clobber each other's writes. Windows surfaced the race;
/// POSIX just got lucky. In CI the `ARKNET_TEST_STORIES260K` env var
/// is set by the workflow so the mutex is never actually contended,
/// but local `cargo test` runs still benefit from it.
fn ensure_fixture() -> Option<PathBuf> {
    static FETCH_LOCK: Mutex<()> = Mutex::new(());
    let _guard = FETCH_LOCK.lock();

    let path = fixture_path();
    if verify_fixture(&path) {
        return Some(path);
    }
    let _ = std::fs::create_dir_all(fixture_dir());
    let status = std::process::Command::new("curl")
        .args(["-sL", "--fail", "-o", path.to_str()?, STORIES260K_URL])
        .status();
    match status {
        Ok(s) if s.success() && verify_fixture(&path) => Some(path),
        _ => None,
    }
}

fn verify_fixture(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let got = sha256(&bytes);
    hex::encode(got.as_bytes()) == STORIES260K_SHA256
}

struct VecSink(Vec<InferenceEvent>);

impl EventSink for VecSink {
    fn accept(&mut self, event: InferenceEvent) -> bool {
        self.0.push(event);
        true
    }
}

fn run_once(model: &Model, prompt: &str, max_tokens: u32) -> (Vec<i32>, String, StopReason) {
    let mut ctx = Context::new(model, ContextParams::deterministic(512)).expect("context init");
    let tokenizer = Tokenizer::new(model);
    let sampler =
        Sampler::new(InferenceMode::Deterministic, &SamplingParams::GREEDY).expect("sampler init");
    let session = Session::new(&mut ctx, tokenizer, sampler);

    let mut sink = VecSink(Vec::new());
    let outcome = session
        .run(
            &SessionRequest {
                prompt: prompt.to_string(),
                max_tokens,
                mode: InferenceMode::Deterministic,
                stop: Vec::new(),
            },
            &mut sink,
        )
        .expect("session run");

    (outcome.generated_tokens, outcome.text, outcome.reason)
}

#[test]
fn deterministic_mode_is_byte_identical_across_runs() {
    let Some(path) = ensure_fixture() else {
        eprintln!(
            "stories260K fixture unavailable and could not be fetched; skipping. \
             Set ARKNET_TEST_STORIES260K to a pre-placed path to force the test."
        );
        return;
    };

    let model = Arc::new(
        Model::load_from_file(&path, ModelLoadParams::deterministic()).expect("model load"),
    );

    let prompt = "Once upon a time";
    let max_tokens = 32;

    let (tokens_a, text_a, _) = run_once(&model, prompt, max_tokens);
    let (tokens_b, text_b, _) = run_once(&model, prompt, max_tokens);

    assert!(!tokens_a.is_empty(), "expected non-empty generation");
    assert_eq!(
        tokens_a, tokens_b,
        "determinism violated: token streams differ"
    );
    assert_eq!(text_a, text_b, "determinism violated: decoded text differs");

    // Print the generated completion so humans can sanity-check the
    // output while inspecting CI logs.
    eprintln!("deterministic output: {text_a:?}");
}

#[test]
fn stop_string_terminates_generation() {
    let Some(path) = ensure_fixture() else {
        eprintln!("fixture unavailable; skipping");
        return;
    };
    let model = Model::load_from_file(&path, ModelLoadParams::deterministic()).expect("model load");

    let mut ctx = Context::new(&model, ContextParams::deterministic(512)).unwrap();
    let tokenizer = Tokenizer::new(&model);
    let sampler = Sampler::new(InferenceMode::Deterministic, &SamplingParams::GREEDY).unwrap();
    let session = Session::new(&mut ctx, tokenizer, sampler);

    let mut sink = VecSink(Vec::new());
    let outcome = session
        .run(
            &SessionRequest {
                prompt: "Once upon a time".into(),
                max_tokens: 64,
                mode: InferenceMode::Deterministic,
                stop: vec![" ".to_string()], // very common — will fire fast
            },
            &mut sink,
        )
        .unwrap();

    assert!(
        matches!(outcome.reason, StopReason::StopString(_)),
        "expected stop-string termination, got {:?}",
        outcome.reason
    );
}

#[test]
fn cancellation_stops_generation() {
    let Some(path) = ensure_fixture() else {
        eprintln!("fixture unavailable; skipping");
        return;
    };
    let model = Model::load_from_file(&path, ModelLoadParams::deterministic()).expect("model load");

    let mut ctx = Context::new(&model, ContextParams::deterministic(512)).unwrap();
    let tokenizer = Tokenizer::new(&model);
    let sampler = Sampler::new(InferenceMode::Deterministic, &SamplingParams::GREEDY).unwrap();
    let session = Session::new(&mut ctx, tokenizer, sampler);

    struct CancelAfter {
        n: u32,
        seen: u32,
    }
    impl EventSink for CancelAfter {
        fn accept(&mut self, event: InferenceEvent) -> bool {
            if matches!(event, InferenceEvent::Token(_)) {
                self.seen += 1;
                self.seen < self.n
            } else {
                true
            }
        }
    }

    let mut sink = CancelAfter { n: 3, seen: 0 };
    let outcome = session
        .run(
            &SessionRequest {
                prompt: "Once upon a time".into(),
                max_tokens: 32,
                mode: InferenceMode::Deterministic,
                stop: Vec::new(),
            },
            &mut sink,
        )
        .unwrap();

    assert!(matches!(outcome.reason, StopReason::Cancelled));
    assert_eq!(sink.seen, 3);
}
