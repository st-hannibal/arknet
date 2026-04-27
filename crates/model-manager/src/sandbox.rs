//! Sandbox stub.
//!
//! Phase 2 fills this in with landlock (Linux), seccomp (Linux), and
//! macOS sandbox_init. The model file opened through [`prepare`] is
//! supposed to live under a restricted filesystem view so a malicious
//! GGUF payload cannot read the rest of the node's state.
//!
//! For Phase 0 this is a passthrough that returns the file path
//! unchanged but centralizes the API so the inference crate can depend
//! on it today without knowing what it will become.

use std::path::{Path, PathBuf};

/// A model prepared for loading by the inference engine.
///
/// In Phase 0 the inner path is identical to what was passed in. In
/// Phase 2 it becomes a path into a restricted filesystem view with
/// landlock / seccomp applied.
#[derive(Clone, Debug)]
pub struct SandboxedModel {
    path: PathBuf,
}

impl SandboxedModel {
    /// Path to feed to llama.cpp's `llama_model_load_from_file`.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Produce a sandboxed view of the on-disk model. Phase 0: identity.
pub fn prepare(path: &Path) -> SandboxedModel {
    SandboxedModel {
        path: path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_returns_same_path_for_now() {
        let p = PathBuf::from("/tmp/fake.gguf");
        let s = prepare(&p);
        assert_eq!(s.path(), p);
    }
}
