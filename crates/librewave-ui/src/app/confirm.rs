//! A dialog that asks before a change that cannot be undone.
use super::Shell;
use super::controls::{Tone, action_button, caption};
use super::theme::{PANEL, RADIUS_CARD, SPACE_LG, SPACE_SM, SPACE_XL, TITLE, TITLE_LINE};
use gpui::{BoxShadow, Context, FontWeight, IntoElement, div, point, prelude::*, px, rgb, rgba};
use librewave_core::Command;

/// A daemon request that waits for the person to confirm it.
pub(super) struct Confirmation {
    pub(super) title: String,
    pub(super) detail: String,
    /// The label of the button that sends the request.
    pub(super) action: &'static str,
    pub(super) command: Command,
    pub(super) focus: gpui::FocusHandle,
}

impl Shell {
    /// Shows the dialog. The request is sent only after confirmation.
    pub(super) fn ask(
        &mut self,
        title: impl Into<String>,
        detail: impl Into<String>,
        action: &'static str,
        command: Command,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.confirmation = Some(Confirmation {
            title: title.into(),
            detail: detail.into(),
            action,
            command,
            focus,
        });
        cx.notify();
    }

    pub(super) fn cancel_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.confirmation.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn accept_confirmation(&mut self, cx: &mut Context<Self>) {
        if let Some(confirmation) = self.confirmation.take() {
            self.send(confirmation.command, cx);
        }
    }

    pub(super) fn confirmation_overlay(
        &self,
        confirmation: &Confirmation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("confirmation-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x0000_0099))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cancel_confirmation(cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("confirmation")
                    .debug_selector(|| "confirmation".to_owned())
                    .track_focus(&confirmation.focus)
                    .occlude()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(400.))
                    .p(px(SPACE_XL))
                    .rounded(px(RADIUS_CARD))
                    .bg(rgb(PANEL))
                    .shadow(vec![BoxShadow {
                        color: gpui::black().opacity(0.6),
                        offset: point(px(0.), px(24.)),
                        blur_radius: px(64.),
                        spread_radius: px(0.),
                    }])
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_LG))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(SPACE_SM))
                            .child(
                                div()
                                    .text_size(px(TITLE))
                                    .line_height(px(TITLE_LINE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(confirmation.title.clone()),
                            )
                            .child(caption(confirmation.detail.clone())),
                    )
                    .child(
                        div()
                            .pt(px(SPACE_SM))
                            .flex()
                            .justify_end()
                            .gap(px(SPACE_SM))
                            .child(action_button(
                                "confirmation-cancel",
                                "Cancel",
                                Tone::Secondary,
                                true,
                                cx,
                                |this, _, cx| this.cancel_confirmation(cx),
                            ))
                            .child(action_button(
                                "confirmation-accept",
                                confirmation.action,
                                Tone::Danger,
                                !self.busy(),
                                cx,
                                |this, _, cx| this.accept_confirmation(cx),
                            )),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn reset_asks_first_and_cancel_keeps_settings_open_without_a_request(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::app::tests::shell_with;
        let (view, cx) = cx.add_window_view(|_, _| shell_with(librewave_core::Snapshot::empty()));
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_settings(window, cx)));
        let reset = cx.debug_bounds("reset-configuration").expect("reset button");
        cx.simulate_click(reset.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("confirmation").is_some());
        view.read_with(cx, |view, _| {
            assert!(view.settings_modal.is_some());
            assert!(!view.pending, "asking sends nothing");
        });
        let cancel = cx.debug_bounds("confirmation-cancel").expect("cancel button");
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert!(view.confirmation.is_none());
            assert!(view.settings_modal.is_some());
            assert!(!view.pending);
        });
        assert!(cx.debug_bounds("confirmation").is_none());
    }
}
