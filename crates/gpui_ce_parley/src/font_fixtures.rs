pub(crate) struct FontFixture {
    pub(crate) family: &'static str,
    pub(crate) data: &'static [u8],
}

pub(crate) const IBM_PLEX: FontFixture = FontFixture {
    family: "IBM Plex Sans",
    data: include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
};

pub(crate) const LILEX: FontFixture = FontFixture {
    family: "Lilex",
    data: include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf"),
};

pub(crate) const SOURCE_SERIF: FontFixture = FontFixture {
    family: "Source Serif 4",
    data: include_bytes!("../../../assets/fonts/source-serif-4/SourceSerif4[opsz,wght].ttf"),
};
