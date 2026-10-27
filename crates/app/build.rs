// Windows resources: the .exe's icon, its file details and its manifest.
//
// The manifest asks for per-monitor DPI awareness (so the interface stays
// sharp on scaled displays) and for version 6 of the common controls, which
// the "open file" dialog needs to look like the rest of Windows.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <assemblyIdentity type="win32" name="BabuTela.App" version="1.0.0.0"/>
    <dependency>
        <dependentAssembly>
            <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
        </dependentAssembly>
    </dependency>
    <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
        <security>
            <requestedPrivileges>
                <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
            </requestedPrivileges>
        </security>
    </trustInfo>
    <asmv3:application>
        <asmv3:windowsSettings>
            <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
            <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2,permonitor</dpiAwareness>
            <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
            <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
        </asmv3:windowsSettings>
    </asmv3:application>
</assembly>
"#;

/// Country flags: Windows draws them as two letters ("BR"), so the app
/// carries its own. The font ships as WOFF2 (78 KB) and egui reads plain
/// TrueType, so it is unpacked here, once, into OUT_DIR/flags.ttf.
fn flag_font() {
    let woff2 = "assets/fonts/TwemojiCountryFlags.woff2";
    println!("cargo:rerun-if-changed={woff2}");
    let packed = std::fs::read(woff2).unwrap_or_else(|e| panic!("lendo {woff2}: {e}"));
    let ttf = wuff::decompress_woff2(&packed).unwrap_or_else(|e| panic!("abrindo {woff2}: {e:?}"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("flags.ttf"), ttf).expect("gravando flags.ttf");
}

/// Google's Noto Color Emoji, for the emoji Windows does not draw yet
/// (src/emoji_font.rs): the COLRv1 font of the Unicode 18.0 release, kept
/// as .xz (2.2 MB instead of 4.8) and unpacked here into OUT_DIR/noto.ttf.
fn noto_font() {
    use std::io::Read as _;
    let xz = "assets/fonts/NotoColorEmoji-COLRv1.ttf.xz";
    println!("cargo:rerun-if-changed={xz}");
    let packed = std::fs::read(xz).unwrap_or_else(|e| panic!("lendo {xz}: {e}"));
    let mut ttf = Vec::with_capacity(packed.len() * 3);
    lzma_rust2::XzReader::new(&packed[..], false)
        .read_to_end(&mut ttf)
        .unwrap_or_else(|e| panic!("abrindo {xz}: {e}"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("noto.ttf"), ttf).expect("gravando noto.ttf");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BABUTELA_SERVER_URL");
    flag_font();
    noto_font();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = "../../build/windows/icon.ico";
    println!("cargo:rerun-if-changed={icon}");
    // The version lives only in the workspace Cargo.toml; the installer
    // reads it back from the .exe's details, so write it plainly.
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon)
        .set_manifest(MANIFEST)
        .set("ProductName", babutela_name())
        .set(
            "FileDescription",
            "Chat de comunidades com texto, voz e vídeo",
        )
        .set("CompanyName", babutela_name())
        .set(
            "LegalCopyright",
            "© 2026 Babu Tela. Todos os direitos reservados.",
        )
        .set("OriginalFilename", "Babu Tela.exe")
        .set("InternalName", "babutela")
        .set("ProductVersion", &version)
        .set("FileVersion", &version);
    if let Err(e) = res.compile() {
        panic!("gravando os recursos do Windows no .exe: {e}");
    }
}

/// Kept in step with `babutela_buildinfo::NAME`, which a build script of the
/// app's own crate cannot depend on.
fn babutela_name() -> &'static str {
    "Babu Tela"
}
