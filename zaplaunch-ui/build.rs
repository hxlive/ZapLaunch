fn main() {
    println!("cargo:rerun-if-changed=../../ZapLaunch-256.png");
    let logo = image::open("../../ZapLaunch-256.png").expect("无法读取 app/ZapLaunch-256.png");
    assert_eq!((logo.width(), logo.height()), (256, 256), "Logo 原图必须为 256×256");
    slint_build::compile("ui/panel.slint").expect("slint build failed");
}
