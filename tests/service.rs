use std::{fs, process::Command};
use vardr::{service, store::Store};

#[test]
fn scheduler_helper_survives_app_replacement_and_refuses_redirection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Store::open(root.join("data")).unwrap();
    let bundle = root.join("Vardr.app");
    fs::create_dir(&bundle).unwrap();
    let source = bundle.join("vardr");
    fs::copy(env!("CARGO_BIN_EXE_vardr"), &source).unwrap();
    let stable = service::stage_helper(&store, &source).unwrap();
    assert_eq!(stable, store.root.join("bin/vardr"));
    let plan = service::plan(&store).unwrap();
    assert!(plan.contains(stable.to_str().unwrap()));
    assert!(!plan.contains(bundle.to_str().unwrap()));
    fs::remove_dir_all(&bundle).unwrap();
    let output = Command::new(&stable).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        concat!("vardr ", env!("CARGO_PKG_VERSION"))
    );

    let before = fs::read(&stable).unwrap();
    let older = root.join("older");
    fs::write(&older, "#!/bin/sh\necho 'vardr 0.0.0'\n").unwrap();
    use std::os::unix::fs::{PermissionsExt, symlink};
    fs::set_permissions(&older, fs::Permissions::from_mode(0o755)).unwrap();
    service::stage_helper(&store, &older).unwrap();
    assert_eq!(fs::read(&stable).unwrap(), before);
    fs::remove_file(&stable).unwrap();
    symlink(&older, &stable).unwrap();
    assert!(service::stage_helper(&store, &older).is_err());
    assert_eq!(
        fs::read_to_string(older).unwrap(),
        "#!/bin/sh\necho 'vardr 0.0.0'\n"
    );
}
