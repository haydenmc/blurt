//! On Windows (MSVC), gives blurt.exe its version info, and its icon when
//! BLURT_WINDOWS_ICON names a .ico (scripts/windows-package.sh makes one
//! from packaging/icons). Explorer, shortcuts and Task Manager show these.
//! Compiled with llvm-rc (cross builds) or rc (Windows), with no crates.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BLURT_WINDOWS_ICON");
    println!("cargo:rerun-if-env-changed=RC");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let abi = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || abi != "msvc" {
        return;
    }

    let icon = env::var_os("BLURT_WINDOWS_ICON").map(PathBuf::from);
    let mut rc = String::new();
    if let Some(icon) = &icon {
        println!("cargo:rerun-if-changed={}", icon.display());
        let path = icon.to_string_lossy().replace('\\', "\\\\");
        rc.push_str(&format!("1 ICON \"{path}\"\n"));
    }
    rc.push_str(&version_info());

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let script = out.join("blurt.rc");
    let res = out.join("blurt.res");
    fs::write(&script, rc).expect("writing blurt.rc");

    let compilers = match env::var("RC") {
        Ok(rc) if !rc.is_empty() => vec![rc],
        _ => vec!["llvm-rc".to_owned(), "rc".to_owned()],
    };
    let compiled = compilers.iter().any(|compiler| {
        Command::new(compiler)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg(&script)
            .status()
            .is_ok_and(|status| status.success())
    });
    if compiled {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else if icon.is_some() {
        // A packaged build asked for the icon: don't ship one without it.
        panic!("couldn't compile Windows resources with {compilers:?}; install llvm-rc");
    } else {
        println!(
            "cargo:warning=no resource compiler ({compilers:?}): blurt.exe has no version info"
        );
    }
}

/// A VERSIONINFO block, from the crate's version. Numeric constants stand
/// in for winver.h's names, so no SDK headers are needed.
fn version_info() -> String {
    let part = |name| {
        env::var(name)
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0)
    };
    let (major, minor, patch) = (
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH"),
    );
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    format!(
        r#"1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "blurt"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "blurt"
      VALUE "LegalCopyright", "AGPL-3.0-or-later"
      VALUE "OriginalFilename", "blurt.exe"
      VALUE "ProductName", "blurt"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
