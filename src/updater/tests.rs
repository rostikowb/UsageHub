use super::*;

#[test]
fn configured_https_transport_does_not_panic() {
    crate::https_test::assert_tls_handshake(super::build_agent().expect("HTTP agent should build"));
}

#[test]
#[ignore = "requires direct Internet access to github.com and badssl.com"]
fn configured_https_transport_validates_certificates() {
    crate::https_test::assert_https_certificate_validation(
        build_agent().expect("HTTP agent should build"),
    );
}

const ABC_DIGEST: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn helper_args() -> Vec<String> {
    [
        "helper.exe",
        "--apply-update",
        "target.exe",
        "source.exe",
        "1234",
        "3",
        ABC_DIGEST,
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn helper_requires_integrity_metadata() {
    let args = helper_args();
    let (target, source, pid, integrity) = parse_apply_update_args(&args).unwrap();
    assert_eq!(target, PathBuf::from("target.exe"));
    assert_eq!(source, PathBuf::from("source.exe"));
    assert_eq!(pid, 1234);
    assert_eq!(integrity.digest_arg(), ABC_DIGEST);
    assert_eq!(integrity.size_arg(), "3");
    for len in 0..args.len() {
        assert!(parse_apply_update_args(&args[..len]).is_err());
    }
    let mut extra = args;
    extra.push("unexpected".into());
    assert!(parse_apply_update_args(&extra).is_err());
}

#[test]
fn helper_rejects_invalid_process_size_and_hash_arguments() {
    for (index, value) in [
        (4, "0"),
        (4, "bad"),
        (5, "0"),
        (5, "-1"),
        (5, "104857601"),
        (6, "sha256:00"),
    ] {
        let mut args = helper_args();
        args[index] = value.into();
        assert!(parse_apply_update_args(&args).is_err());
    }
}

#[test]
fn corrupt_update_never_replaces_the_installed_binary() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("download.exe");
    let target = dir.path().join("app.exe");
    std::fs::write(&source, b"abd").unwrap();
    std::fs::write(&target, b"original").unwrap();
    let integrity = AssetIntegrity::new(3, Some(ABC_DIGEST)).unwrap();
    let error = apply_update(target.clone(), source, 0, &integrity).unwrap_err();
    assert!(error.contains("SHA-256"));
    assert_eq!(std::fs::read(&target).unwrap(), b"original");
    assert!(!backup_path_for(&target).exists());
}

#[test]
fn verified_source_can_replace_the_installed_binary() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("download.exe");
    let target = dir.path().join("app.exe");
    std::fs::write(&source, b"abc").unwrap();
    std::fs::write(&target, b"original").unwrap();
    let integrity = AssetIntegrity::new(3, Some(ABC_DIGEST)).unwrap();
    let _guard = open_verified_source(&source, &integrity).unwrap();
    replace_target_binary(&target, &source).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"abc");
    assert!(!backup_path_for(&target).exists());
}

#[test]
fn updater_agent_rejects_plain_http() {
    let error = build_agent()
        .unwrap()
        .get("http://127.0.0.1:1/update.exe")
        .call()
        .unwrap_err();
    assert!(matches!(error, ureq::Error::RequireHttpsOnly(_)));
}

#[test]
fn is_winget_install_path_accepts_configured_roots_and_descendants() {
    // Read the configured roots without mutating process-wide environment variables.
    let roots = winget_install_roots();
    assert!(!roots.is_empty());
    for root in roots {
        assert!(is_winget_install_path(&root), "root: {}", root.display());
        let executable = root.join("UsageHub.UsageHub_source").join("app.exe");
        let path = executable.to_string_lossy();
        for variant in [
            path.to_string(),
            path.to_ascii_uppercase(),
            path.replace('\\', "/"),
            format!("{path}\\"),
            format!(r"\\?\{path}"),
        ] {
            assert!(
                is_winget_install_path(Path::new(&variant)),
                "path: {variant}"
            );
        }
    }
}

#[test]
fn is_winget_install_path_rejects_siblings_with_the_same_prefix() {
    for root in winget_install_roots() {
        for suffix in ["-old", "Backup", ".old"] {
            let sibling = format!("{}{suffix}\\app.exe", root.display());
            assert!(
                !is_winget_install_path(Path::new(&sibling)),
                "path: {sibling}"
            );
        }
        assert!(!is_winget_install_path(root.parent().unwrap()));
    }
}

#[test]
fn is_winget_install_path_rejects_portable_and_empty_paths() {
    for path in [
        "",
        "app.exe",
        r"C:\Portable\app.exe",
        r"C:\Downloads\WinGet\Packages\app.exe",
    ] {
        assert!(!is_winget_install_path(Path::new(path)), "path: {path:?}");
    }
}

#[test]
fn portable_checks_return_github_results_and_errors() {
    let release = ReleaseDescriptor {
        latest_version: "99.0.0".into(),
        asset_url: "https://github.com/rostikowb/UsageHub/releases/download/v99.0.0/usagehub.exe"
            .into(),
        integrity: AssetIntegrity::new(3, Some(ABC_DIGEST)).unwrap(),
    };
    let result = check_channel_updates(InstallChannel::Portable, || Ok(Some(release))).unwrap();
    let UpdateCheckResult::Available(AvailableUpdate::Release(release)) = result else {
        panic!("Expected a portable release");
    };
    assert_eq!(release.latest_version, "99.0.0");
    assert_eq!(release.integrity.digest_arg(), ABC_DIGEST);
    assert!(matches!(
        check_channel_updates(InstallChannel::Portable, || Ok(None)).unwrap(),
        UpdateCheckResult::UpToDate
    ));
    assert_eq!(
        check_channel_updates(
            InstallChannel::Portable,
            || Err("GitHub unavailable".into())
        )
        .unwrap_err(),
        "GitHub unavailable"
    );
}

#[test]
fn winget_install_never_queries_github_or_offers_a_fake_upgrade() {
    let error = check_channel_updates(InstallChannel::Winget, || {
        panic!("WinGet installs must not be replaced with a portable release")
    })
    .unwrap_err();
    assert_eq!(error, winget_unavailable());
    assert_eq!(begin_winget_update().unwrap_err(), error);
}
