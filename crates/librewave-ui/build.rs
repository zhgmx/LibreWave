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
        let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        fs::write(out.join("wave3.bgra"), frame(&pixmap)).expect("build output must be writable");
        render_knob(&tree, WIDTH as f32 / tree.size().width(), &out);
    }

    /// Extracts the opaque cap from the same SVG. Placement comes from its
    /// canvas bounds, not a second copy of the knob geometry in UI code.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    fn render_knob(tree: &usvg::Tree, scale: f32, out: &std::path::Path) {
        let node = tree.node_by_id("knob-face").expect("the illustration has a foreground cap");
        let bounds = node.abs_layer_bounding_box().expect("the knob has nonzero bounds");
        // The checked-in SVG bounds are positive and fit within the small art canvas.
        let width = (bounds.width() * scale).ceil() as u32;
        let height = (bounds.height() * scale).ceil() as u32;
        let mut pixmap = tiny_skia::Pixmap::new(width, height).expect("non-zero knob canvas");
        let usvg::Node::Group(group) = node else {
            panic!("the foreground cap must remain one SVG group");
        };
        let parent = group
            .abs_transform()
            .pre_concat(group.transform().invert().expect("the knob transform must be invertible"));
        // render_node crops in canvas coordinates but renders only this node's
        // relative transform. Apply its ancestors (including the viewBox) after
        // that crop. The final translation cancels render_node's own crop.
        let transform = tiny_skia::Transform::from_scale(scale, scale)
            .pre_translate(-bounds.x(), -bounds.y())
            .pre_concat(parent)
            .pre_translate(bounds.x(), bounds.y());
        resvg::render_node(node, transform, &mut pixmap.as_mut())
            .expect("the foreground cap renders");
        fs::write(out.join("wave3-knob.bgra"), frame(&pixmap))
            .expect("knob output must be writable");
        fs::write(
            out.join("wave3-knob.rs"),
            format!(
                "const KNOB_PIXELS: (u32, u32) = ({width}, {height});\n\
                 #[allow(clippy::unreadable_literal)]\n\
                 const KNOB_BOUNDS: (f32, f32, f32, f32) = ({:?}, {:?}, {:?}, {:?});\n",
                bounds.x(),
                bounds.y(),
                width as f32 / scale,
                height as f32 / scale,
            ),
        )
        .expect("knob placement must be writable");
    }

    // GPUI frames hold straight-alpha BGRA.
    fn frame(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
        let mut frame = Vec::with_capacity(pixmap.data().len());
        for pixel in pixmap.pixels() {
            let color = pixel.demultiply();
            frame.extend_from_slice(&[color.blue(), color.green(), color.red(), color.alpha()]);
        }
        frame
    }
}
