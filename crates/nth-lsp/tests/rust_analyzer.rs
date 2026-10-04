//! The whole path against a real rust-analyzer. Needs it on PATH, and is
//! slow while it indexes, so run it by hand:
//! `cargo test -p nth-lsp -- --ignored`.

use std::time::Instant;

use nth_lsp::{Lsp, LspConfig, ServerState, report};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rust-analyzer on PATH"]
async fn reports_a_type_error() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let file = project.join("src/main.rs");
    std::fs::write(&file, "fn main() {\n    let x: u32 = \"text\";\n}\n").unwrap();

    let lsp = Lsp::new(&LspConfig::default());
    // One touch on a cold server: the client waits for rust-analyzer to
    // load the crate rather than trusting its first, empty answer.
    let started = Instant::now();
    let diagnostics = lsp.touch(&file, true).await;
    println!("cold touch took {:?}", started.elapsed());
    let text = report::after_write(&file, &diagnostics);

    println!("{text}");
    assert!(
        text.contains("LSP errors detected in this file, please fix:"),
        "{text}"
    );
    assert!(text.contains("ERROR [2:"), "{text}");
    let status = lsp.status().borrow().clone();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].id, "rust");
    assert_eq!(status[0].root, project);
    assert_eq!(status[0].state, ServerState::Connected);
}
