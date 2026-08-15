use gpui::{
    App, Bounds, Context, FontWeight, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};

const BACKGROUND: u32 = 0x000D_0F11;
const PANEL: u32 = 0x0014_171A;
const PANEL_RAISED: u32 = 0x001A_1E22;
const BORDER: u32 = 0x0026_2B30;
const INK: u32 = 0x00F4_F5F6;
const MUTED: u32 = 0x0099_A1AA;
const SUBTLE: u32 = 0x006E_767F;
const ACCENT: u32 = 0x0068_D5C0;
const CAUTION: u32 = 0x00E9_B872;

/// Open the optional desktop application shell.
///
/// # Panics
///
/// Panics if GPUI cannot open its initial window.
pub fn run() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(960.), px(640.))),
            app_id: Some("dev.librewave.LibreWave".to_owned()),
            ..WindowOptions::default()
        };

        cx.open_window(options, |_, cx| cx.new(|_| Shell))
            .expect("failed to open the LibreWave application window");
    });
}

struct Shell;

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(INK))
            .flex()
            .flex_col()
            .child(header())
            .child(div().flex_1().min_h_0().flex().child(sidebar()).child(workspace()))
    }
}

fn header() -> impl IntoElement {
    div()
        .h(px(68.))
        .px(px(24.))
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .size(px(28.))
                        .rounded(px(8.))
                        .bg(rgb(ACCENT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(BACKGROUND))
                        .font_weight(FontWeight::BOLD)
                        .child("L"),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("LibreWave"))
                        .child(div().text_xs().text_color(rgb(MUTED)).child("Audio mixer")),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(9.))
                .px(px(12.))
                .py(px(7.))
                .rounded(px(8.))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(div().size(px(7.)).rounded_full().bg(rgb(CAUTION)))
                .child(div().text_sm().text_color(rgb(MUTED)).child("Device not connected")),
        )
}

fn sidebar() -> impl IntoElement {
    div()
        .w(px(190.))
        .p(px(16.))
        .border_r_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .gap(px(22.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(section_label("MIX"))
                .child(navigation_item("Monitor mix", true))
                .child(navigation_item("Stream mix", false)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(section_label("HARDWARE"))
                .child(navigation_item("Device settings", false)),
        )
        .child(div().flex_1())
        .child(
            div()
                .p(px(12.))
                .rounded(px(9.))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .flex()
                .flex_col()
                .gap(px(5.))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Preview shell"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(SUBTLE))
                        .child("Hardware and audio controls are disabled."),
                ),
        )
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().px(px(9.)).text_xs().text_color(rgb(SUBTLE)).child(label)
}

fn navigation_item(label: &'static str, selected: bool) -> impl IntoElement {
    div()
        .h(px(36.))
        .px(px(10.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .gap(px(9.))
        .when(selected, |element| element.bg(rgb(PANEL_RAISED)).text_color(rgb(INK)))
        .when(!selected, |element| element.text_color(rgb(MUTED)))
        .child(
            div()
                .size(px(5.))
                .rounded_full()
                .when(selected, |element| element.bg(rgb(ACCENT)))
                .when(!selected, |element| element.bg(rgb(SUBTLE))),
        )
        .child(div().text_sm().child(label))
}

fn workspace() -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .p(px(24.))
        .flex()
        .flex_col()
        .gap(px(20.))
        .child(
            div()
                .flex()
                .items_end()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child(
                            div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Monitor mix"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child("Set what you hear through the Wave headphone output."),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(SUBTLE))
                        .child("Connect a supported Wave device to start."),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .gap(px(16.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .gap(px(12.))
                        .child(channel_strip("Microphone", "Wave input"))
                        .child(channel_strip("System", "Desktop audio"))
                        .child(channel_strip("Voice chat", "Application input"))
                        .child(channel_strip("Music", "Application input")),
                )
                .child(output_panel()),
        )
}

fn channel_strip(name: &'static str, detail: &'static str) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(126.))
        .h_full()
        .p(px(14.))
        .rounded(px(10.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(5.))
        .child(div().w_full().font_weight(FontWeight::SEMIBOLD).text_sm().child(name))
        .child(div().w_full().text_xs().text_color(rgb(SUBTLE)).child(detail))
        .child(
            div()
                .flex_1()
                .min_h(px(220.))
                .pt(px(24.))
                .pb(px(18.))
                .flex()
                .items_center()
                .gap(px(16.))
                .child(
                    div().w(px(7.)).h_full().max_h(px(280.)).rounded_full().bg(rgb(PANEL_RAISED)),
                )
                .child(
                    div()
                        .relative()
                        .w(px(4.))
                        .h_full()
                        .max_h(px(280.))
                        .rounded_full()
                        .bg(rgb(BORDER))
                        .child(
                            div()
                                .absolute()
                                .left(px(-7.))
                                .bottom(px(46.))
                                .w(px(18.))
                                .h(px(8.))
                                .rounded(px(4.))
                                .bg(rgb(SUBTLE)),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .py(px(8.))
                .rounded(px(7.))
                .bg(rgb(PANEL_RAISED))
                .text_center()
                .text_xs()
                .text_color(rgb(SUBTLE))
                .child("Unavailable"),
        )
}

fn output_panel() -> impl IntoElement {
    div()
        .w(px(232.))
        .h_full()
        .p(px(16.))
        .rounded(px(10.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Outputs"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(SUBTLE))
                        .child("Hardware and software levels stay separate."),
                ),
        )
        .child(output_row("Headphones", "Hardware level"))
        .child(output_row("Stream mix", "Software level"))
        .child(div().flex_1())
        .child(
            div()
                .p(px(12.))
                .rounded(px(8.))
                .bg(rgb(PANEL_RAISED))
                .text_xs()
                .text_color(rgb(MUTED))
                .child("The desktop volume control will not change microphone gain or headphone hardware level."),
        )
}

fn output_row(name: &'static str, detail: &'static str) -> impl IntoElement {
    div()
        .p(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name))
        .child(div().text_xs().text_color(rgb(SUBTLE)).child(detail))
        .child(div().mt(px(7.)).h(px(5.)).rounded_full().bg(rgb(PANEL_RAISED)))
}
