use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::PathBuf,
};

use calloop::{LoopHandle, PostAction};
use filedescriptor::Pipe;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::{
    WaylandClientStatePtr,
    platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout},
};
use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Image, ImageFormat, hash};
use smallvec::SmallVec;
use url::Url;

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";

fn file_uri_list(paths: &ExternalPaths) -> String {
    paths
        .0
        .iter()
        .filter_map(|path| Url::from_file_path(path).ok())
        .fold(String::new(), |mut list, url| {
            list.push_str(url.as_str());
            list.push_str("\r\n");
            list
        })
}

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    self_mime: String,

    // Internal clipboard
    contents: Option<ClipboardItem>,
    primary_contents: Option<ClipboardItem>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = Pipe::new().unwrap();
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().unwrap();

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                log::error!("error reading clipboard pipe: {err:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|&mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|&allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                log::error!("Failed to convert clipboard content to UTF-8: {}", e);
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for mime_type in &self.mime_types {
            let Some(format) = ImageFormat::from_mime_type(mime_type) else {
                continue;
            };

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }

    fn read_paths(&self, connection: &Connection) -> Option<ClipboardItem> {
        if !self.has_mime_type(FILE_LIST_MIME_TYPE) {
            return None;
        }
        let bytes = self.read_bytes(connection, FILE_LIST_MIME_TYPE)?;
        let text = String::from_utf8(bytes).ok()?;
        let paths = text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| Url::parse(line).ok()?.to_file_path().ok())
            .collect::<SmallVec<[PathBuf; 2]>>();
        if paths.is_empty() {
            return None;
        }
        Some(ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(paths))],
        })
    }
}

impl Clipboard {
    pub fn new(
        connection: Connection,
        loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    ) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    pub fn set(&mut self, item: ClipboardItem) {
        self.contents = Some(item);
    }

    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(item);
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    pub fn mime_types(&self) -> Vec<String> {
        let Some(contents) = self.contents.as_ref() else {
            return Vec::new();
        };
        let mut mime_types = Vec::new();
        if contents
            .entries
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::String(_)))
        {
            mime_types.extend(TEXT_MIME_TYPES.map(str::to_owned));
        }
        if contents.entries.iter().any(|entry| {
            matches!(entry, ClipboardEntry::ExternalPaths(paths) if !file_uri_list(paths).is_empty())
        }) {
            mime_types.push(FILE_LIST_MIME_TYPE.to_owned());
        }
        for entry in &contents.entries {
            let ClipboardEntry::Image(image) = entry else {
                continue;
            };
            let mime_type = image.format.mime_type().to_owned();
            if !mime_types.contains(&mime_type) {
                mime_types.push(mime_type);
            }
        }
        mime_types
    }

    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        let Some(contents) = self.contents.as_ref() else {
            return;
        };
        self.send_item(contents, &mime_type, fd);
    }

    pub fn send_primary(&self, mime_type: String, fd: OwnedFd) {
        let Some(contents) = self.primary_contents.as_ref() else {
            return;
        };
        self.send_item(contents, &mime_type, fd);
    }

    fn send_item(&self, item: &ClipboardItem, mime_type: &str, fd: OwnedFd) {
        let bytes = if TEXT_MIME_TYPES.contains(&mime_type) {
            Some(
                item.entries
                    .iter()
                    .filter_map(|entry| match entry {
                        ClipboardEntry::String(value) => Some(value.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
                    .into_bytes(),
            )
        } else if mime_type == FILE_LIST_MIME_TYPE {
            item.entries.iter().find_map(|entry| match entry {
                ClipboardEntry::ExternalPaths(paths) => {
                    let list = file_uri_list(paths);
                    (!list.is_empty()).then(|| list.into_bytes())
                }
                _ => None,
            })
        } else {
            ImageFormat::from_mime_type(mime_type).and_then(|format| {
                item.entries.iter().find_map(|entry| match entry {
                    ClipboardEntry::Image(image) if image.format == format => {
                        Some(image.bytes.clone())
                    }
                    _ => None,
                })
            })
        };
        if let Some(bytes) = bytes {
            self.send_bytes(fd, bytes);
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))
            .or_else(|| offer.read_paths(&self.connection))?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.primary_contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))
            .or_else(|| offer.read_paths(&self.connection))?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        let mut written = 0;
        self.loop_handle
            .insert_source(
                calloop::generic::Generic::new(
                    File::from(fd),
                    calloop::Interest::WRITE,
                    calloop::Mode::Level,
                ),
                move |_, file, _| {
                    let file = unsafe { file.get_mut() };
                    loop {
                        match file.write(&bytes[written..]) {
                            Ok(n) if written + n == bytes.len() => {
                                written += n;
                                break Ok(PostAction::Remove);
                            }
                            Ok(n) => written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                break Ok(PostAction::Continue);
                            }
                            Err(_) => break Ok(PostAction::Remove),
                        }
                    }
                },
            )
            .unwrap();
    }
}
