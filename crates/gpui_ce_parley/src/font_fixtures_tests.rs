pub(crate) const IBM_PLEX_SEMIBOLD: FontFixture = FontFixture {
    family: IBM_PLEX.family,
    data: include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
};

pub(crate) const IBM_PLEX_SEMIBOLD_ITALIC: FontFixture = FontFixture {
    family: IBM_PLEX.family,
    data: include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
};

pub(crate) const NOTO_COLOR_EMOJI: FontFixture = FontFixture {
    family: "Noto Color Emoji",
    data: include_bytes!("../../../assets/fonts/noto-color-emoji/NotoColorEmoji.subset.ttf"),
};
