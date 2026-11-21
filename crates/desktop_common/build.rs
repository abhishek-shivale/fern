fn main() {
    println!("cargo:rerun-if-changed=native/calendar.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .file("native/calendar.m")
            .compile("fern_calendar_macos");
        for framework in [
            "EventKit",
            "Foundation",
            "Network",
            "Security",
            "UserNotifications",
        ] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
