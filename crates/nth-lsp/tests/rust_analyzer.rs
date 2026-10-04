//! The whole path against a real rust-analyzer. Needs it on PATH, and is
//! slow while it indexes, so run it by hand:
//! `cargo test -p nth-lsp -- --ignored`.

use std::time::Duration;

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
    // rust-analyzer only checks once it has loaded the crate, which can
    // take longer than one touch waits.
    let mut text = String::new();
    for _ in 0..30 {
        let diagnostics = lsp.touch(&file, true).await;
        text = report::after_write(&file, &diagnostics);
        if !text.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

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
