fn main() {
    let library = pkg_config::Config::new()
        .atleast_version("1.6")
        .probe("gupnp-1.6")
        .expect("GUPnP 1.6 development files are required");
    let mut build = cc::Build::new();
    build.file("native/upnp.c").include("native");
    for path in library.include_paths {
        build.include(path);
    }
    build
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("radiod_upnp");
    println!("cargo:rerun-if-changed=native/upnp.c");
    println!("cargo:rerun-if-changed=native/upnp.h");
    println!("cargo:rerun-if-changed=native/upnp");
}
