//! Check the privacy identity before the updater replaces an installed bundle.
use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use std::{
    io::Cursor,
    path::{Component, Path},
};

fn extract(bytes: &[u8], destination: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let components: Vec<_> = path.components().collect();
        ensure!(
            matches!(components.first(), Some(Component::Normal(name)) if *name == "Vardr.app"),
            "update archive must contain only Vardr.app"
        );
        ensure!(components.iter().all(|part| matches!(part, Component::Normal(name)
                    if !name.to_string_lossy().starts_with("._") && !name.to_string_lossy().contains('\\'))),
                "update archive contains unsafe metadata or paths");
        ensure!(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "update archive contains links or special files"
        );
        ensure!(
            entry.unpack_in(destination)?,
            "update archive entry was not extracted"
        );
    }
    Ok(())
}

pub(crate) fn verify_archive(installed: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    extract(bytes, temporary.path())?;
    let candidate = temporary.path().join("Vardr.app");
    vardr::macos_signing::verify_update_identity(installed, &candidate)
        .context("desktop update changes the macOS privacy identity; installed app preserved")?;
    vardr::macos_signing::verify_update_identity(
        &installed.join("Contents/MacOS/vardr"),
        &candidate.join("Contents/MacOS/vardr"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn archive(path: &str, kind: tar::EntryType) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_entry_type(kind);
        header.set_size(0);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("outside").unwrap();
        }
        header.set_cksum();
        builder.append_data(&mut header, path, &[][..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn rejects_links_special_files_metadata_and_wrong_roots() {
        let directory = tempfile::tempdir().unwrap();
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Fifo,
        ] {
            assert!(extract(&archive("Vardr.app/link", kind), directory.path()).is_err());
        }
        for name in ["outside/file", "Vardr.app/._file", "Vardr.app/back\\slash"] {
            assert!(extract(&archive(name, tar::EntryType::Regular), directory.path()).is_err());
        }
        extract(
            &archive("Vardr.app/file", tar::EntryType::Regular),
            directory.path(),
        )
        .unwrap();
        assert!(directory.path().join("Vardr.app/file").is_file());
    }
}
