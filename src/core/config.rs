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
        #[cfg(test)]
        assert_scratch_config_dir("Config::load");
        Self(CoreConfig::load())
    }

    /// Test-only guard that shadows [`CoreConfig::save`].
    ///
    /// `save` is a *full* overwrite of `config.json`, and the config dir is
    /// resolved process-wide from `$HOME` unless a test pins it. A GUI test that
    /// forgets to pin therefore doesn't just leak a file — it resets the
    /// developer's entire live config to whatever the test built (this is not
    /// hypothetical: the keybinding tests in `ui::app` did exactly that, which is
    /// how this guard came to exist). An inherent method wins over the `Deref` to
    /// [`CoreConfig`], so every `cfg.save()` in the crate routes through here
    /// under `cargo test` and through the core method otherwise — no call site
    /// has to opt in.
    ///
    /// Pin a scratch dir with [`pin_test_config_dir`] in the test's harness.
    #[cfg(test)]
    pub fn save(&self) {
        assert_scratch_config_dir("Config::save");
        self.0.save();
    }
}

/// Whether `dir` is the platform's real per-user config dir — the one a
/// developer's own tty7 reads and writes.
///
/// `None` (nothing resolves — no `$HOME`) is not "real": IO there is a no-op, so
/// there is nothing to protect.
#[cfg(test)]
fn is_real_user_config_dir(dir: Option<&std::path::Path>) -> bool {
    dir.is_some() && dir == default_config_dir().as_deref()
}

/// Panic unless the config dir has been pinned away from the developer's real
/// one. See [`Config::save`] for why.
#[cfg(test)]
fn assert_scratch_config_dir(what: &str) {
    assert!(
        !is_real_user_config_dir(config_dir_path().as_deref()),
        "{what} in a test would touch the real user config dir ({}). \
         Call `crate::core::config::pin_test_config_dir()` \
         at the top of the test/harness first.",
        config_dir_path().unwrap_or_default().display(),
    );
}

/// Point this process's config dir at a scratch directory, so config-dir IO in
/// tests can't reach the developer's real `~/.config/tty7`.
///
/// Every test in the binary must pin **this same path**. `set_config_dir` is
/// first-call-wins and process-wide, so a test that pinned a scratch dir of its
/// own would silently redirect whichever tests lost the race away from the
/// directory they then read back — one shared path makes the race outcome
/// irrelevant. That is why this takes no name: the single call site for the
/// path is the point.
#[cfg(test)]
pub(crate) fn pin_test_config_dir() {
    let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    set_config_dir(dir);
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
    use std::path::Path;

    /// The guard behind [`Config::save`]: it has to recognize the real dir (so a
    /// forgotten pin is caught) and clear a scratch one (so pinned tests run).
    /// Testing the predicate rather than the panic keeps this independent of
    /// which test pinned the process first — `set_config_dir` is first-call-wins,
    /// so an unpinned state can't be staged once any test has run.
    #[test]
    fn the_real_config_dir_is_the_only_one_the_guard_rejects() {
        // Whatever the platform resolves to for this user is exactly what tests
        // must never write to.
        if let Some(real) = default_config_dir() {
            assert!(is_real_user_config_dir(Some(&real)));
            // A scratch dir under it is still not *it* — the guard compares the
            // dir itself, not an ancestor.
            assert!(!is_real_user_config_dir(Some(&real.join("scratch"))));
        }
        assert!(!is_real_user_config_dir(Some(Path::new(
            "/tmp/tty7-scratch"
        ))));
        // Nothing resolves (no `$HOME`) → config IO is a no-op, nothing to guard.
        assert!(!is_real_user_config_dir(None));
    }

    /// The pin helper must land somewhere the guard accepts — otherwise every
    /// harness that follows this advice would still panic.
    #[test]
    fn pinning_lands_outside_the_real_config_dir() {
        pin_test_config_dir();
        assert!(!is_real_user_config_dir(config_dir_path().as_deref()));
    }

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
