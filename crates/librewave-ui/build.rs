//! Draws the device illustration once at build time. The application then
//! shows it on the first frame without a vector renderer on the startup path.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/devices/wave3.svg");
    #[cfg(feature = "gpui")]
    device_art::render();
}

#[cfg(feature = "gpui")]
mod device_art {
    use resvg::{tiny_skia, usvg};
    use std::{env, fs, path::PathBuf};

    // Twice the largest size the application shows. Keep in step with `device_art.rs`.
    const WIDTH: u32 = 720;
    const HEIGHT: u32 = 1080;

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn render() {
        let svg = fs::read("assets/devices/wave3.svg").expect("device illustration must exist");
        let tree = usvg::Tree::from_data(&svg, &usvg::Options::default())
            .expect("device illustration must be valid SVG");
        let mut pixmap = tiny_skia::Pixmap::new(WIDTH, HEIGHT).expect("non-zero canvas");
        let scale = tiny_skia::Transform::from_scale(
            WIDTH as f32 / tree.size().width(),
            HEIGHT as f32 / tree.size().height(),
        );
        resvg::render(&tree, scale, &mut pixmap.as_mut());
        // GPUI frames hold straight-alpha BGRA.
        let mut frame = Vec::with_capacity(pixmap.data().len());
        for pixel in pixmap.pixels() {
            let color = pixel.demultiply();
            frame.extend_from_slice(&[color.blue(), color.green(), color.red(), color.alpha()]);
        }
        let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        fs::write(out.join("wave3.bgra"), frame).expect("build output must be writable");
    }
}
