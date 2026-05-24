fn main() {
    // Only embed resources when targeting Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();

        // Version info shown in Explorer → Properties → Details.
        res.set("FileDescription", "CLI tool updater for Windows");
        res.set("ProductName", "clipdate");
        res.set("InternalName", "clipdate.exe");
        res.set("OriginalFilename", "clipdate.exe");

        // CARGO_PKG_VERSION is set automatically from Cargo.toml at build time.
        let version = env!("CARGO_PKG_VERSION");
        res.set("FileVersion", version);
        res.set("ProductVersion", version);

        // Embed an application manifest so Windows knows this is a normal
        // user-level process (no UAC elevation prompt).
        res.set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="clipdate" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 / 11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>"#,
        );

        res.compile().expect("failed to compile Windows resources");
    }
}
