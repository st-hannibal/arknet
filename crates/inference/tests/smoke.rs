//! Smoke test — verifies llama.cpp can load and decode the stories260K
//! fixture. Skipped when the fixture is not available so CI without
//! network still passes; real determinism tests live in
//! `tests/determinism.rs`.

use std::path::PathBuf;

use arknet_inference::{InferenceMode, Model, ModelLoadParams};

fn fixture_path() -> Option<PathBuf> {
    let p = std::env::var("ARKNET_TEST_STORIES260K")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            Some(
                std::env::temp_dir()
                    .join("arknet-test-fixtures")
                    .join("stories260K.gguf"),
            )
        })?;
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
fn load_stories260k_and_read_metadata() {
    let Some(path) = fixture_path() else {
        eprintln!("stories260K.gguf not present; skipping");
        return;
    };

    let _ = InferenceMode::Deterministic;
    let model =
        Model::load_from_file(&path, ModelLoadParams::default()).expect("model should load");

    let n_params = model.n_params();
    let n_vocab = model.n_vocab();
    let n_ctx_train = model.n_ctx_train();
    eprintln!(
        "stories260K: n_params={n_params}, n_vocab={n_vocab}, n_ctx_train={n_ctx_train}, desc={}",
        model.description()
    );

    // Sanity: vocab size > 0, model has at least some parameters.
    assert!(n_vocab > 0, "expected non-empty vocab");
    assert!(n_params > 0, "expected non-zero parameter count");
    assert!(n_ctx_train > 0, "expected non-zero training context");
}
