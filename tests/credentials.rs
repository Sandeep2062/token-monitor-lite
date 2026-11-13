use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use token_monitor_lite::credentials::{
    CredentialError, CredentialMaterial, CredentialService, stable_credential_id,
};

#[test]
#[ignore = "explicit platform credential-store probe"]
fn native_adapter_reports_available_or_unavailable_within_deadline() {
    let service = CredentialService::start().expect("start credential worker");
    let credential_id = stable_credential_id("platform-readiness-probe");
    let started = Instant::now();

    let availability = match service.get(1, &credential_id) {
        Ok(secret) => {
            drop(secret);
            "available"
        }
        Err(CredentialError::NotFound) => "available",
        Err(CredentialError::Unavailable | CredentialError::TimedOut) => "unavailable",
        Err(error) => panic!("unexpected credential adapter result: {error}"),
    };

    assert!(started.elapsed() <= Duration::from_secs(3));
    println!("credential_adapter={availability}");
    service.shutdown().expect("join credential worker");
}

#[test]
#[ignore = "explicit platform credential-store write/read/delete probe"]
fn native_adapter_completes_real_store_lifecycle_without_leaking_secret() {
    if std::env::var_os("TOKEN_MONITOR_LIVE_CREDENTIAL_WRITE").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        println!("credential_lifecycle=not-requested");
        return;
    }

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos()
        ^ u128::from(std::process::id());
    let credential_id = stable_credential_id(&format!("platform-lifecycle-{seed:032x}"));
    let mut secret = opaque_bytes(seed);
    let set_material = CredentialMaterial::from_secret_bytes(secret.clone())
        .expect("generated secret is nonempty");
    let verify_material = CredentialMaterial::from_secret_bytes(secret.clone())
        .expect("generated secret is nonempty");
    let service = CredentialService::start().expect("stage=start failed");
    let started = Instant::now();

    let mut written = false;
    let mut failed_stage = "set";
    let lifecycle = (|| {
        failed_stage = "set";
        let expected = service.set(1, &credential_id, set_material)?;
        written = true;
        failed_stage = "get";
        let actual = service.get(2, &credential_id)?;
        failed_stage = "compare";
        if !actual.matches(&expected) {
            return Err(CredentialError::Unavailable);
        }
        failed_stage = "verify";
        service.verify(3, &credential_id, verify_material)?;
        failed_stage = "delete";
        service.delete(4, &credential_id)?;
        written = false;
        failed_stage = "not-found";
        match service.get(5, &credential_id) {
            Err(CredentialError::NotFound) => Ok(()),
            Ok(_) => Err(CredentialError::Unavailable),
            Err(error) => Err(error),
        }
    })();

    let cleanup = if written {
        match service.delete(100, &credential_id) {
            Ok(()) | Err(CredentialError::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    let shutdown = service.shutdown();
    let leak_free = isolated_home_is_secret_free(&secret);
    secret.fill(0);

    assert!(started.elapsed() <= Duration::from_secs(15));
    assert!(
        lifecycle.is_ok(),
        "credential_lifecycle_stage={failed_stage}-failed"
    );
    assert!(cleanup.is_ok(), "credential_lifecycle_stage=cleanup-failed");
    assert!(
        shutdown.is_ok(),
        "credential_lifecycle_stage=shutdown-failed"
    );
    assert!(leak_free, "credential_lifecycle_stage=secret-leak-detected");
    println!("credential_lifecycle=passed");
}

fn opaque_bytes(mut state: u128) -> Vec<u8> {
    let mut output = vec![0_u8; 32];
    for byte in &mut output {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state as u8) ^ ((state >> 64) as u8);
    }
    output
}

fn isolated_home_is_secret_free(secret: &[u8]) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let home = Path::new(&home);
    if !home.is_absolute() || home.parent().is_none() {
        return false;
    }
    tree_is_secret_free(home, secret, 0, &mut 0_usize) && process_arguments_are_secret_free(secret)
}

fn tree_is_secret_free(path: &Path, secret: &[u8], depth: usize, files: &mut usize) -> bool {
    if depth > 12 || *files > 10_000 {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return true;
    }
    if metadata.is_file() {
        *files += 1;
        if metadata.len() > 64 * 1024 * 1024 {
            return false;
        }
        return fs::read(path)
            .map(|bytes| !contains_bytes(&bytes, secret))
            .unwrap_or(false);
    }
    if !metadata.is_dir() {
        return true;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .all(|entry| tree_is_secret_free(&entry.path(), secret, depth + 1, files))
}

#[cfg(target_os = "linux")]
fn process_arguments_are_secret_free(secret: &[u8]) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    processes.filter_map(Result::ok).all(|process| {
        if !process
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            return true;
        }
        match fs::read(process.path().join("cmdline")) {
            Ok(bytes) => !contains_bytes(&bytes, secret),
            Err(_) => true,
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn process_arguments_are_secret_free(_secret: &[u8]) -> bool {
    true
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
