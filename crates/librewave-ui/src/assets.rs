use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

pub(super) struct Assets;

const ICONS: &[(&str, &[u8])] = &[
    ("mic", include_bytes!("../assets/icons/mic.svg")),
    ("system", include_bytes!("../assets/icons/system.svg")),
    ("headphones", include_bytes!("../assets/icons/headphones.svg")),
    ("stream", include_bytes!("../assets/icons/stream.svg")),
    ("settings", include_bytes!("../assets/icons/settings.svg")),
    ("effects", include_bytes!("../assets/icons/effects.svg")),
    ("link", include_bytes!("../assets/icons/link.svg")),
    ("chevron", include_bytes!("../assets/icons/chevron.svg")),
    ("speaker", include_bytes!("../assets/icons/speaker.svg")),
    ("listen", include_bytes!("../assets/icons/listen.svg")),
    ("plus", include_bytes!("../assets/icons/plus.svg")),
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
    ]
}
