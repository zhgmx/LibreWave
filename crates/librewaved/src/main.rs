fn main() {
    if let Err(error) = librewaved::run(librewaved::socket_path()) {
        eprintln!("librewaved: {error}");
        std::process::exit(1);
    }
}
