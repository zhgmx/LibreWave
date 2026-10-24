use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

pub(super) struct Assets;

const ICONS: &[(&str, &[u8])] = &[
    ("arrow-clockwise", include_bytes!("../assets/icons/arrow-clockwise.svg")),
    ("copy", include_bytes!("../assets/icons/copy.svg")),
    ("folder-open", include_bytes!("../assets/icons/folder-open.svg")),
    ("headphones", include_bytes!("../assets/icons/headphones.svg")),
    ("lock-simple", include_bytes!("../assets/icons/lock-simple.svg")),
    ("mic", include_bytes!("../assets/icons/mic.svg")),
    ("microphone-slash", include_bytes!("../assets/icons/microphone-slash.svg")),
    ("minus", include_bytes!("../assets/icons/minus.svg")),
    ("play", include_bytes!("../assets/icons/play.svg")),
    ("plugs", include_bytes!("../assets/icons/plugs.svg")),
    ("plugs-connected", include_bytes!("../assets/icons/plugs-connected.svg")),
    ("plus", include_bytes!("../assets/icons/plus.svg")),
    ("power", include_bytes!("../assets/icons/power.svg")),
    ("settings", include_bytes!("../assets/icons/settings.svg")),
    ("shield-check", include_bytes!("../assets/icons/shield-check.svg")),
    ("speaker-high", include_bytes!("../assets/icons/speaker-high.svg")),
    ("speaker-slash", include_bytes!("../assets/icons/speaker-slash.svg")),
    ("stop", include_bytes!("../assets/icons/stop.svg")),
    ("stream", include_bytes!("../assets/icons/stream.svg")),
    ("system", include_bytes!("../assets/icons/system.svg")),
    ("warning-circle", include_bytes!("../assets/icons/warning-circle.svg")),
    ("wave-sine", include_bytes!("../assets/icons/wave-sine.svg")),
    ("wrench", include_bytes!("../assets/icons/wrench.svg")),
    ("x", include_bytes!("../assets/icons/x.svg")),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(name, _)| (*name).into()).collect())
    }
}

pub(super) fn fonts() -> Vec<Cow<'static, [u8]>> {
    vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/geist-regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/geist-medium.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/geist-semibold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/geist-mono-regular.ttf")),
    ]
}
