//! Embed the published WeType icon and version into both Windows executables.
//! Windows releases already use an MSVC/Windows SDK developer environment.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets/wetype.ico");
    println!("cargo:rerun-if-env-changed=WXCLIPD_RC");
    assert_eq!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("windows"),
        "wxclipd supports Windows only; build with a Windows MSVC target (release: i686-pc-windows-msvc)"
    );
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ENV").as_deref(),
        Ok("msvc"),
        "Windows releases require the MSVC toolchain"
    );
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let major = env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let minor = env::var("CARGO_PKG_VERSION_MINOR").unwrap();
    let patch = env::var("CARGO_PKG_VERSION_PATCH").unwrap();
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets/wetype.ico");
    let icon = icon
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let directory = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let source = directory.join("wxclipd.rc");
    let resource = directory.join("wxclipd.res");
    std::fs::write(
        &source,
        format!(
            r#"#pragma code_page(65001)
101 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "CoderDoubleflower\0"
            VALUE "FileDescription", "WeType Clipboard Sync\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "ProductName", "WeType Clipboard Sync\0"
            VALUE "ProductVersion", "{version}\0"
            VALUE "Comments", "Unofficial clipboard sync; WeType icon belongs to Tencent.\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
        ),
    )
    .expect("write Windows resource source");
    // WXCLIPD_RC can select a compatible resource compiler for cross checking.
    // A missing/failed compiler is an error: do not silently ship generic icons.
    let compiler = env::var_os("WXCLIPD_RC").unwrap_or_else(|| "rc.exe".into());
    let status = Command::new(&compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&source)
        .status()
        .expect("run Windows SDK rc.exe (use an MSVC developer shell, or set WXCLIPD_RC)");
    assert!(status.success(), "Windows resource compiler failed");
    assert!(
        resource.is_file(),
        "Windows resource compiler produced no .res file"
    );
    // Only the two application binaries receive these resources, not SDK tests.
    println!("cargo:rustc-link-arg-bins={}", resource.display());
}
