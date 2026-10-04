//! The UTF-8 byte order mark. Editors that emit one (Visual Studio,
//! Notepad) expect to keep it, so the tools that write files keep it too.

use std::path::Path;

use tokio::io::AsyncReadExt;

pub(crate) const BOM: &str = "\u{feff}";

pub(crate) async fn has_bom(path: &Path) -> Result<bool, String> {
    let read = async {
        let mut file = tokio::fs::File::open(path).await?;
        let mut head = [0u8; BOM.len()];
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..]).await? {
                0 => break,
                n => filled += n,
            }
        }
        Ok::<_, std::io::Error>(head[..filled] == *BOM.as_bytes())
    };
    read.await
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Adds or drops the BOM so the file has one exactly when `bom` says,
/// after a formatter that does not know about it rewrote the file.
pub(crate) async fn sync(path: &Path, bom: bool) -> std::io::Result<()> {
    let bytes = tokio::fs::read(path).await?;
    let text = bytes.strip_prefix(BOM.as_bytes()).unwrap_or(&bytes);
    if (text.len() != bytes.len()) == bom {
        return Ok(());
    }
    let synced = match bom {
        true => [BOM.as_bytes(), text].concat(),
        false => text.to_vec(),
    };
    tokio::fs::write(path, synced).await
}
