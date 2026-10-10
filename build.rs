fn main() {
    // `gpui_backend`: targets with a GPUI platform layer (desktop, plus
    // Android and iOS with the `mobile` feature). `gpui_android` and
    // `gpui_ios` select the mobile ones; `gpui_mobile` is either of them.
    println!("cargo::rustc-check-cfg=cfg(gpui_backend)");
    println!("cargo::rustc-check-cfg=cfg(gpui_android)");
    println!("cargo::rustc-check-cfg=cfg(gpui_ios)");
    println!("cargo::rustc-check-cfg=cfg(gpui_mobile)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mobile = std::env::var_os("CARGO_FEATURE_MOBILE").is_some();
    let android = os == "android" && mobile;
    let ios = os == "ios" && mobile;
    if !matches!(os.as_str(), "android" | "ios") || android || ios {
        println!("cargo::rustc-cfg=gpui_backend");
    }
    if android {
        println!("cargo::rustc-cfg=gpui_android");
    }
    if ios {
        println!("cargo::rustc-cfg=gpui_ios");
    }
    if android || ios {
        println!("cargo::rustc-cfg=gpui_mobile");
    }

    // Publishes `android/` (the GpuiView SurfaceView and its Tauri plugin
    // class) so the app's Gradle project includes it.
    tauri_plugin::Builder::new(&[])
        .android_path("android")
        .build();
}
