// The installer's build: the app it carries and its Windows resources.
//
// `cargo xtask release` packs the app (see src/payload.rs) and points
// BABUTELA_SETUP_PAYLOAD at the result. Any other build (clippy, tests, a
// plain `cargo build`) gets an empty payload, and that installer says it
// has nothing inside instead of installing anything.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <assemblyIdentity type="win32" name="BabuTela.Setup" version="1.0.0.0"/>
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
    <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
        <application>
            <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
        </application>
    </compatibility>
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

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BABUTELA_SETUP_PAYLOAD");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let payload = match std::env::var("BABUTELA_SETUP_PAYLOAD") {
        Ok(path) if !path.is_empty() => {
            println!("cargo:rerun-if-changed={path}");
            std::path::PathBuf::from(path)
        }
        _ => {
            let empty = out.join("payload-vazio.bin");
            std::fs::write(&empty, b"").expect("gravando o payload vazio");
            empty
        }
    };
    println!("cargo:rustc-env=BABUTELA_PAYLOAD={}", payload.display());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = "../../build/windows/icon.ico";
    println!("cargo:rerun-if-changed={icon}");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut res = winresource::WindowsResource::new();
    // Resource 1: the icon the dialogs show too.
    res.set_icon_with_id(icon, "1")
        .set_manifest(MANIFEST)
        .set("ProductName", "Babu Tela")
        .set("FileDescription", "Instalador do Babu Tela")
        .set("CompanyName", "Babu Tela")
        .set(
            "LegalCopyright",
            "© 2026 Babu Tela. Todos os direitos reservados.",
        )
        .set("OriginalFilename", "Babu Tela Setup.exe")
        .set("InternalName", "babutela-setup")
        .set("ProductVersion", &version)
        .set("FileVersion", &version);
    if let Err(e) = res.compile() {
        panic!("gravando os recursos do Windows no .exe: {e}");
    }
}
