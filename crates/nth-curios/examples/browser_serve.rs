//! Manual browser fixture: cargo run -p nth-curios --example browser_serve -- /tmp/curios-browser
//! Read <directory>/url, touch <directory>/revise to publish r2, touch stop to exit.
use std::{fs, path::PathBuf, process::Command, time::Duration};

use nth_curios::{Host, Publish, Store};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(std::env::args().nth(1).expect("fixture directory"));
    fs::create_dir_all(&directory)?;
    let repo = tempfile::tempdir_in(&directory)?;
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path())
            .status()?
            .success()
    );
    let store = Store::new(repo.path(), "browser-owner")?;
    let html = include_str!("../tests/browser_document.html");
    let published = store.publish(Publish {
        request_id: "browser-r1".into(),
        curio_id: None,
        expected_revision: None,
        title: Some("Browser isolation fixture".into()),
        html: html.into(),
    })?;
    let host = Host::start(store.clone(), &published.curio.id).await?;
    fs::write(directory.join("url"), host.url())?;
    fs::write(
        directory.join("repo"),
        repo.path().to_string_lossy().as_bytes(),
    )?;
    let mut revised = false;
    while !directory.join("stop").exists() {
        if !revised && directory.join("revise").exists() {
            store.publish(Publish {
                request_id: "browser-r2".into(),
                curio_id: Some(published.curio.id.clone()),
                expected_revision: Some(1),
                title: None,
                html: html.replace("id=\"summary\"", "id=\"replacement\""),
            })?;
            revised = true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    host.stop().await?;
    Ok(())
}
