/// `cal --version`, the MCP server's `serverInfo` and the plugin version check report the app's
/// version: the app crate's [package] version, which `pnpm version-packages` keeps in sync.
/// This crate stays at 0.0.0 (see Cargo.toml), so its own CARGO_PKG_VERSION would say 0.0.0.
fn main() {
    println!("cargo:rerun-if-changed=../Cargo.toml");
    let manifest = std::fs::read_to_string("../Cargo.toml").expect("read ../Cargo.toml");
    let Some(version) = package_version(&manifest) else {
        panic!(
            "no `version = \"...\"` in the [package] table of ../Cargo.toml; \
             cal --version and the MCP serverInfo are stamped from it"
        );
    };
    println!("cargo:rustc-env=CALLIMACHUS_VERSION={version}");
}

/// The `version` of the `[package]` table only, as sync-versions.mjs scopes its replace:
/// a `[workspace.package]` or dependency table above it must not win. Tolerates any
/// whitespace around `=` and a trailing `# comment`.
fn package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line.split('#').next().map(str::trim) == Some("[package]");
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue; // a key like `version-foo`
        };
        let value = value.trim_start().strip_prefix('"')?;
        let end = value.find('"')?;
        return Some(value[..end].to_string());
    }
    None
}
