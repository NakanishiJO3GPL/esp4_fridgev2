fn main() {
    println!("cargo:rerun-if-changed=idf");
    println!("cargo:rerun-if-changed=sdkconfig.defaults");
    println!("cargo:rerun-if-changed=wifi.conf");
    if let Ok(contents) = std::fs::read_to_string("wifi.conf") {
        let mut ssid = None;
        let mut password = None;
        for line in contents.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').expect("wifi.conf: expected key=value");
            let target = match key.trim() {
                "ssid" => &mut ssid,
                "password" => &mut password,
                _ => panic!("wifi.conf: only ssid and password keys are supported"),
            };
            assert!(target.is_none(), "wifi.conf: duplicate key");
            assert!(!value.contains('\0'), "wifi.conf: NUL is not allowed");
            *target = Some(value);
        }
        println!(
            "cargo:rustc-env=FRIDGE_WIFI_SSID={}",
            ssid.expect("wifi.conf: missing ssid")
        );
        println!(
            "cargo:rustc-env=FRIDGE_WIFI_PASSWORD={}",
            password.expect("wifi.conf: missing password")
        );
    }
    embuild::espidf::sysenv::output();
}
