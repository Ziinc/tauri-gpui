fn main() {
    // `gpui_backend`: targets with a GPUI platform layer (desktop, plus
    // Android with the `mobile` feature). `gpui_android`: the Android one.
    println!("cargo::rustc-check-cfg=cfg(gpui_backend)");
    println!("cargo::rustc-check-cfg=cfg(gpui_android)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mobile = std::env::var_os("CARGO_FEATURE_MOBILE").is_some();
    let android = os == "android" && mobile;
    if !matches!(os.as_str(), "android" | "ios") || android {
        println!("cargo::rustc-cfg=gpui_backend");
    }
    if android {
        println!("cargo::rustc-cfg=gpui_android");
    }

    // Publishes `android/` (the GpuiView SurfaceView and its Tauri plugin
    // class) so the app's Gradle project includes it.
    tauri_plugin::Builder::new(&[])
        .android_path("android")
        .build();
}
