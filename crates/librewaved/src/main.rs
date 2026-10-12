fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let result = match arguments.as_slice() {
        [] => librewaved::run(librewaved::socket_path()),
        [argument] if argument == "--check-config" => {
            librewaved::check_config().map_err(std::io::Error::other)
        }
        [argument] if argument == "--help" || argument == "-h" => {
            println!(
                "Usage: librewaved [--check-config]\n\n--check-config validates profiles without starting the daemon or changing files."
            );
            Ok(())
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unknown daemon option; use --help",
        )),
    };
    if let Err(error) = result {
        eprintln!("librewaved: {error}");
        std::process::exit(1);
    }
}
