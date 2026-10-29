fn main() {
    slint_build::compile("ui/app.slint").expect("编译 Slint 界面失败");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("app.res");
        let status = std::process::Command::new("rc.exe")
            .arg("/nologo").arg("/I").arg("assets").arg("/fo").arg(&out).arg("assets/app.rc")
            .status().expect("找不到 Windows 资源编译器，请使用 build.ps1 编译");
        assert!(status.success(), "编译 Windows 图标资源失败");
        println!("cargo:rustc-link-arg={}", out.display());
    }
}

