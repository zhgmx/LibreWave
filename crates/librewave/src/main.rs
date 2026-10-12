fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let view = match args.as_slice() {
        [] => librewave_ui::LaunchView::Mixer,
        [value] if value == "--settings" => librewave_ui::LaunchView::Settings,
        [value] if matches!(value.as_str(), "--help" | "-h") => {
            println!(
                "Usage: librewave [--settings]\n\n  --settings  Open device and audio settings"
            );
            return;
        }
        _ => {
            eprintln!("Usage: librewave [--settings]");
            std::process::exit(2);
        }
    };
    librewave_ui::run(view);
}
