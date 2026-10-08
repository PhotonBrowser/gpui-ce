use crate::{App, Global, Window, WindowAppearance, rgb};
use palette::rgb::Rgba;
use std::ops::Deref;
use std::sync::Arc;

const SELECTED_TEXT_WEIGHT: f32 = 0.37;

fn selected_color(text: Rgba, background: Rgba) -> Rgba {
    let mix_channel = |text: f32, background: f32| {
        text * SELECTED_TEXT_WEIGHT + background * (1.0 - SELECTED_TEXT_WEIGHT)
    };

    Rgba::new(
        mix_channel(text.color.red, background.color.red),
        mix_channel(text.color.green, background.color.green),
        mix_channel(text.color.blue, background.color.blue),
        1.0,
    )
}

/// The default set of colors for gpui.
///
/// These are used for styling base components, examples and more.
#[derive(Clone, Debug)]
pub struct Colors {
    /// Text color
    pub text: Rgba,
    /// Selected text color
    pub selected_text: Rgba,
    /// Background color
    pub background: Rgba,
    /// Disabled color
    pub disabled: Rgba,
    /// Selected color
    pub selected: Rgba,
    /// Border color
    pub border: Rgba,
    /// Separator color
    pub separator: Rgba,
    /// Container color
    pub container: Rgba,
}

impl Default for Colors {
    fn default() -> Self {
        Self::light()
    }
}

impl Colors {
    /// Returns the default colors for the given window appearance.
    pub fn for_appearance(window: &Window) -> Self {
        match window.appearance() {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
        }
    }

    /// Returns the default dark colors.
    pub fn dark() -> Self {
        let text = rgb(0xffffff);
        let background = rgb(0x222222);
        Self {
            text,
            selected_text: text,
            disabled: rgb(0x565656),
            selected: selected_color(text, background),
            background,
            border: rgb(0x000000),
            separator: rgb(0xd9d9d9),
            container: rgb(0x262626),
        }
    }

    /// Returns the default light colors.
    pub fn light() -> Self {
        let text = rgb(0x252525);
        let background = rgb(0xffffff);
        Self {
            text,
            selected_text: text,
            background,
            disabled: rgb(0xb0b0b0),
            selected: selected_color(text, background),
            border: rgb(0xd9d9d9),
            separator: rgb(0xe6e6e6),
            container: rgb(0xf4f5f5),
        }
    }

    /// Get [Colors] from the global state
    pub fn get_global(cx: &App) -> &Arc<Colors> {
        &cx.global::<GlobalColors>().0
    }
}

/// Get [Colors] from the global state
#[derive(Clone, Debug)]
pub struct GlobalColors(pub Arc<Colors>);

impl Deref for GlobalColors {
    type Target = Arc<Colors>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Global for GlobalColors {}

/// Implement this trait to allow global [Colors] access via `cx.default_colors()`.
pub trait DefaultColors {
    /// Returns the default [`Colors`]
    fn default_colors(&self) -> &Arc<Colors>;
}

impl DefaultColors for App {
    fn default_colors(&self) -> &Arc<Colors> {
        &self.global::<GlobalColors>().0
    }
}

/// The appearance of the base GPUI colors, used to style GPUI elements
///
/// Varies based on the system's current [`WindowAppearance`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAppearance {
    /// Use the set of colors for light appearances.
    #[default]
    Light,
    /// Use the set of colors for dark appearances.
    Dark,
}

impl From<WindowAppearance> for DefaultAppearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}
