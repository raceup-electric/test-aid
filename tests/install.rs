//! Exercise installation and replacement without writing to the real user's PATH.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use taid::fixture::TempDir;

#[test]
fn rerunning_installer_replaces_existing_binary_and_preserves_path_configuration() {
    let temp = TempDir::new("installer-update").unwrap();
    let source = temp.path().join("source-taid");
    let destination = temp.path().join("bin");
    fs::create_dir(&destination).unwrap();
    let installer = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let path = format!(
        "{}:{}",
        destination.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let install = || {
        let output = Command::new("sh")
            .arg(&installer)
            .arg("--bin")
            .arg(&source)
            .env("TAID_INSTALL_DIR", &destination)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    fs::write(&source, "#!/bin/sh\nprintf 'old version\\n'\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(install().contains("Installed"));
    let binary = destination.join("taid");
    assert_eq!(fs::read(&binary).unwrap(), fs::read(&source).unwrap());

    fs::write(&source, "#!/bin/sh\nprintf 'new version\\n'\n").unwrap();
    let result = install();
    assert!(result.contains("Updated"));
    assert!(result.contains("already on PATH"));
    assert_eq!(fs::read(&binary).unwrap(), fs::read(&source).unwrap());
    let output = Command::new(&binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"new version\n");
    assert!(
        install().contains("Updated"),
        "same binary can be installed again"
    );
    assert_eq!(
        fs::read_dir(&destination).unwrap().count(),
        1,
        "temporary staging files are removed"
    );
}
