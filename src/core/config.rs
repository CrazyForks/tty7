//! The gpui-facing half of the configuration model.
//!
//! Every field, every default, every parse rule and all of the `config.json` IO
//! live in `tty7-core` — the daemon and the headless server read the same file
//! and must agree with the GUI byte for byte, and neither of them links gpui.
//! Two things are left here, and both exist only because they *are* gpui:
//!
//! 1. **[`Config`] as a global.** gpui keys its global map by type and
//!    `gpui::Global` is a foreign trait, so it cannot be implemented for the
//!    core struct from this crate. [`Config`] is therefore a transparent
//!    newtype around [`tty7_core::core::config::Config`] that carries the
//!    `Global` impl; it `Deref`s to the core struct, so `cx.global::<Config>()
//!    .font_size` and friends read exactly as they always did.
//! 2. **[`gpui_font_features`]**, which converts the stored feature list into
//!    the `gpui::FontFeatures` the text system wants.
//!
//! Everything else is re-exported unchanged, so `crate::core::config::…` still
//! resolves to the same items across the whole GUI.

// Everything else — every enum, helper and constant — passes straight through,
// so `crate::core::config::…` resolves exactly as it did before the split. The
// `Config` this glob would bring in is shadowed by the newtype below.
pub use tty7_core::core::config::*;

/// The core configuration struct, under a name this module's own [`Config`]
/// wrapper doesn't shadow.
pub use tty7_core::core::config::Config as CoreConfig;

/// The app's live configuration, as gpui holds it: a newtype over
/// [`CoreConfig`] whose only job is to carry the `gpui::Global` impl the orphan
/// rule won't let us put on the core struct directly.
///
/// It `Deref`s (and `DerefMut`s) to the core struct, so reads and writes go
/// through untouched — `cx.global::<Config>().font_size`,
/// `cx.global_mut::<Config>().window_blur = Some(on)`,
/// `cx.global::<Config>().save()`. Construct one with [`Config::load`],
/// `Config::default()`, or `Config(core_config)`.
#[derive(Debug, Clone, Default)]
pub struct Config(pub CoreConfig);

impl gpui::Global for Config {}

impl Config {
    /// Load the config, falling back to defaults if the file is absent or
    /// unreadable — see [`CoreConfig::load`].
    pub fn load() -> Self {
        Self(CoreConfig::load())
    }
}

impl std::ops::Deref for Config {
    type Target = CoreConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Config {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<CoreConfig> for Config {
    fn from(config: CoreConfig) -> Self {
        Self(config)
    }
}

/// The configured OpenType features, in the shape gpui's text system takes.
///
/// The stored form is a `tty7-core` replica of `gpui::FontFeatures` with an
/// identical wire format (see [`FontFeatures`]); this is the one place the two
/// meet, so the conversion — and the test below that pins their serializations
/// together — is all that keeps them honest.
pub fn gpui_font_features(features: &FontFeatures) -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(features.tag_value_list().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `font_features` is a real key in the user's `config.json`, and the type
    /// backing it moved out of gpui when the core crate split off. The two must
    /// still parse the same JSON to the same feature list and write it back
    /// identically — otherwise the split silently rewrote user config.
    #[test]
    fn font_features_match_gpui_byte_for_byte() {
        const JSON: &str = r#"{"calt":true,"liga":1,"ss01":0,"zero":false,"bad":1,"kern":null}"#;

        let ours: FontFeatures = serde_json::from_str(JSON).unwrap();
        let theirs: gpui::FontFeatures = serde_json::from_str(JSON).unwrap();

        assert_eq!(ours.tag_value_list(), theirs.tag_value_list());
        assert_eq!(ours.is_calt_enabled(), theirs.is_calt_enabled());
        assert_eq!(
            serde_json::to_string(&ours).unwrap(),
            serde_json::to_string(&theirs).unwrap()
        );
        assert_eq!(gpui_font_features(&ours), theirs);
    }
}
