fn main() {
    println!("cargo:rerun-if-changed=src/audio_host/pipewire_filter_shim.c");
    println!("cargo:rerun-if-changed=src/audio_host/pipewire_filter_shim.h");

    let library = pkg_config::Config::new()
        .atleast_version("0.3")
        .probe("libpipewire-0.3")
        .expect("libpipewire-0.3 development files are required");
    let mut build = cc::Build::new();
    build
        .file("src/audio_host/pipewire_filter_shim.c")
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .flag_if_supported("-std=c11");
    for include in library.include_paths {
        build.include(include);
    }
    build.compile("librewave_pipewire_filter_shim");
}
