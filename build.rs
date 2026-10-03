fn main() {
    // Windows: request administrator rights at launch (raw disk access) and set icon metadata.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#,
        );
        // A complete version resource. Antivirus heuristics score a binary with no company,
        // no copyright and no description as more suspicious than one that identifies itself.
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        res.set("ProductName", "Rockchip SD Tool");
        res.set("FileDescription", "Rockchip SD Tool: writes Rockchip RKFW firmware images to SD cards");
        res.set("CompanyName", "TheGammaSqueeze");
        res.set("LegalCopyright", "Copyright (c) TheGammaSqueeze. MIT License.");
        res.set("OriginalFilename", "rockchip_sd_tool.exe");
        res.set("InternalName", "rockchip_sd_tool");
        res.set("Comments", "Source: https://github.com/TheGammaSqueeze/rockchip_sd_tool");
        res.set("ProductVersion", &version);
        res.set("FileVersion", &version);
        if std::path::Path::new("assets/icon.ico").exists() {
            res.set_icon("assets/icon.ico");
        }
        if let Err(e) = res.compile() {
            println!("cargo:warning=windows resource compile failed: {e}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
