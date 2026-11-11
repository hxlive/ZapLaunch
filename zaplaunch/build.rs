use std::{env, fs, path::PathBuf, process::Command};

fn rc_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', "\\r").replace('\n', "\\n")
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("../../ZapLaunch.ico").canonicalize().unwrap();
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-env-changed=ZAPLAUNCH_BUILD_DATE");
    let build_date = env::var("ZAPLAUNCH_BUILD_DATE").unwrap_or_else(|_| {
        let output = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "Get-Date -Format yyMMdd"]).output().expect("无法读取构建日期");
        assert!(output.status.success(), "读取构建日期失败");
        String::from_utf8(output.stdout).expect("构建日期编码错误").trim().to_owned()
    });
    assert!(build_date.len() == 6 && build_date.bytes().all(|b| b.is_ascii_digit()), "构建日期须为六位 yyMMdd");
    let display_version = format!("{} Beta · Build {build_date}", env!("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=ZAPLAUNCH_DISPLAY_VERSION={display_version}");


    // 图标和版本信息共用一个资源对象，不增加运行时依赖。
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let rc = out.join("zaplaunch-icon.rc");
    let obj = out.join("zaplaunch-icon.o");
    let rc_path = rc.to_string_lossy().replace('\\', "/");
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    let [major, minor, patch] = [env!("CARGO_PKG_VERSION_MAJOR"), env!("CARGO_PKG_VERSION_MINOR"), env!("CARGO_PKG_VERSION_PATCH")]
        .map(|part| part.parse::<u16>().expect("Windows 版本号各段必须在 0..65535 范围内"));
    let version = &display_version;
    let author = rc_string(env!("CARGO_PKG_AUTHORS"));
    let description = rc_string(env!("CARGO_PKG_DESCRIPTION"));
    let flags = if env::var("PROFILE").as_deref() == Ok("release") { 2 } else { 3 };
    fs::write(&rc, format!(r#"#pragma code_page(65001)
1 ICON "{icon_path}"
LANGUAGE 0x04, 0x02
1 VERSIONINFO
 FILEVERSION {major},{minor},{patch},0
 PRODUCTVERSION {major},{minor},{patch},0
 FILEFLAGSMASK 0x3fL
 FILEFLAGS {flags}
 FILEOS 0x40004L
 FILETYPE 0x1L
 FILESUBTYPE 0x0L
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "080404B0"
  BEGIN
   VALUE "CompanyName", "{author}\0"
   VALUE "FileDescription", "ZapLaunch {description}\0"
   VALUE "FileVersion", "{version}\0"
   VALUE "InternalName", "zaplaunch\0"
   VALUE "LegalCopyright", "Copyright © 2026 {author}\0"
   VALUE "OriginalFilename", "zaplaunch.exe\0"
   VALUE "ProductName", "ZapLaunch\0"
   VALUE "ProductVersion", "{version}\0"
   VALUE "Comments", "常驻托盘，呼出即用 · GPL-3.0\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0804, 1200
 END
END
"#)).unwrap();

    let status = Command::new("windres")
        .arg("--input")
        .arg(&rc)
        .arg("--output")
        .arg(&obj)
        .arg("--output-format=coff")
        .arg("--codepage=65001")
        .status()
        .unwrap_or_else(|e| panic!("无法执行 windres（MinGW 工具必须在 PATH 中）：{e}"));
    assert!(status.success(), "windres 编译应用资源失败：{rc_path}");

    println!("cargo:rustc-link-arg-bin=zaplaunch={}", obj.display());
    println!("cargo:rustc-link-arg-bin=zaplaunch=-Wl,--gc-sections");
}
