fn main() {
    let mut output = std::io::stdout().lock();
    let mut errors = std::io::stderr().lock();
    match librewavectl::run_default(std::env::args(), &mut output, &mut errors) {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("librewavectl: {error}");
            std::process::exit(1);
        }
    }
}
