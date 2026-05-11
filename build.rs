fn main() {
    println!("cargo:rerun-if-env-changed=DOCKAGENTS_BUILD_VERSION");
    if let Ok(version) = std::env::var("DOCKAGENTS_BUILD_VERSION") {
        println!("cargo:rustc-env=DOCKAGENTS_BUILD_VERSION={version}");
    }
}
