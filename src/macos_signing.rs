//! Preserve the macOS identity against which privacy permissions were granted.
use anyhow::{Context, Result};
use std::{path::Path, time::Duration};

fn requirement(path: &Path) -> Result<String> {
    let output = crate::process::capture(
        &[
            "/usr/bin/codesign".into(),
            "--display".into(),
            "--requirements".into(),
            "-".into(),
            path.to_string_lossy().into_owned(),
        ],
        path.parent()
            .context("executable has no parent directory")?,
        16 * 1024,
        Duration::from_secs(10),
    )?;
    std::str::from_utf8(&output)?
        .lines()
        .find_map(|line| {
            line.strip_prefix("designated => ")
                .or_else(|| line.strip_prefix("# designated => "))
        })
        .map(str::to_owned)
        .context("macOS executable has no designated signing requirement")
}

pub(crate) fn verify_update_identity(installed: &Path, candidate: &Path) -> Result<()> {
    let installed_requirement = requirement(installed)?;
    let candidate_requirement = requirement(candidate)?;
    // Both directions matter: the candidate must retain existing grants, and a
    // later release must not inherit a broader requirement from this update.
    for (path, required) in [
        (candidate, installed_requirement),
        (installed, candidate_requirement),
    ] {
        crate::process::capture(
            &[
                "/usr/bin/codesign".into(),
                "--verify".into(),
                "--strict".into(),
                "--test-requirement".into(),
                format!("={required}"),
                path.to_string_lossy().into_owned(),
            ],
            path.parent().context("executable has no parent directory")?,
            1024,
            Duration::from_secs(10),
        )
        .context("macOS update would change the signing identity used for privacy permissions; installed executable preserved; use an explicit signing-identity migration instead")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    fn run(args: &[&str]) {
        let output = Command::new(args[0]).args(&args[1..]).output().unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture(directory: &Path, name: &str, value: u8) -> std::path::PathBuf {
        let source = directory.join(format!("{name}.c"));
        let binary = directory.join(name);
        fs::write(&source, format!("int main(void) {{ return {value}; }}\n")).unwrap();
        run(&[
            "/usr/bin/clang",
            source.to_str().unwrap(),
            "-o",
            binary.to_str().unwrap(),
        ]);
        run(&[
            "/usr/bin/codesign",
            "--force",
            "--sign",
            "-",
            "--identifier",
            "dev.vardr.signing-test",
            binary.to_str().unwrap(),
        ]);
        binary
    }

    #[test]
    fn changed_ad_hoc_build_cannot_replace_permission_identity() {
        let directory = tempfile::tempdir().unwrap();
        let installed = fixture(directory.path(), "installed", 1);
        let candidate = fixture(directory.path(), "candidate", 2);
        let before = fs::read(&installed).unwrap();
        let error = verify_update_identity(&installed, &candidate).unwrap_err();
        assert!(error.to_string().contains("privacy permissions"));
        assert_eq!(fs::read(&installed).unwrap(), before);
    }

    #[test]
    fn matching_signature_is_accepted_and_tampering_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let installed = fixture(directory.path(), "installed", 1);
        let candidate = directory.path().join("candidate");
        fs::copy(&installed, &candidate).unwrap();
        verify_update_identity(&installed, &candidate).unwrap();
        let mut bytes = fs::read(&candidate).unwrap();
        bytes[0] ^= 1;
        fs::write(&candidate, bytes).unwrap();
        assert!(verify_update_identity(&installed, &candidate).is_err());
    }
}
