fn main() {
    #[cfg(windows)]
    windows_resource();
}

/// Embed what `tauri_build::build()` embeds for the app's own bins: the app icon, version info
/// and the Common-Controls v6 manifest, and link the VC runtime statically (see Cargo.toml). Built natively on the Windows runner,
/// so the host cfg is the target cfg.
#[cfg(windows)]
fn windows_resource() {
    use tauri_winres::{VersionInfo, WindowsResource};

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    println!("cargo:rerun-if-changed=../Cargo.toml");
    println!("cargo:rerun-if-changed=../icons/icon.ico");
    // The app crate's [package] version, which `pnpm version-packages` keeps in sync; this
    // crate stays at 0.0.0 so the sync script and Cargo.lock never need to know about it.
    let manifest = std::fs::read_to_string("../Cargo.toml").expect("read ../Cargo.toml");
    let version = manifest
        .lines()
        .find_map(|l| {
            l.strip_prefix("version = \"")
                .and_then(|r| r.strip_suffix('"'))
        })
        .expect("version in ../Cargo.toml");
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let mut packed = 0u64;
    for _ in 0..3 {
        packed = (packed << 16) | parts.next().unwrap_or(0);
    }
    packed <<= 16;

    let mut res = WindowsResource::new();
    res.set_manifest(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*" />
    </dependentAssembly>
  </dependency>
</assembly>"#,
    );
    res.set_version_info(VersionInfo::FILEVERSION, packed);
    res.set_version_info(VersionInfo::PRODUCTVERSION, packed);
    res.set("FileVersion", version);
    res.set("ProductVersion", version);
    res.set("ProductName", "Callimachus");
    res.set("CompanyName", "shaller");
    res.set("FileDescription", "Callimachus");
    res.set_icon("../icons/icon.ico");
    res.compile().expect("compile the Windows resource");
    // Static VC runtime, as tauri-build gives the app's own bins (see Cargo.toml); CI checks
    // with dumpbin that cal.exe does not import VCRUNTIME140.dll.
    static_vcruntime::metabuild();
}
