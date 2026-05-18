fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/encoder.ico");
    println!("cargo:rerun-if-env-changed=ENCODER_ICON");

    let mut res = winres::WindowsResource::new();

    let icon = if std::env::var("ENCODER_ICON").ok().as_deref() == Some("1") {
        "assets/encoder.ico"
    } else {
        "assets/app.ico"
    };

    res.set_icon(icon);

    // Embed a manifest to enable ComCtl32 v6.
    //
    // Use processorArchitecture="*" so Windows selects the Common-Controls
    // assembly that matches the actual EXE architecture. Keep the XML
    // declaration out of the embedded manifest to avoid parser issues in
    // tools that inspect SxS manifests from resources.
    res.set_manifest(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>

  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>

  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">false</dpiAware>
      <gdiScaling xmlns="http://schemas.microsoft.com/SMI/2017/WindowsSettings">true</gdiScaling>
    </windowsSettings>
  </application>
</assembly>"#,
    );

    res.compile().unwrap();
}
