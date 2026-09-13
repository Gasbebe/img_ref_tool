#![cfg(target_os = "windows")]

//! Windows counterpart to `platform_macos`. Browsers commonly drag images
//! out without ever placing a real file path (`CF_HDROP`) on the drop data
//! object. Confirmed by logging every format Chrome actually offers during
//! an image drag: no `CF_HDROP`, no registered "PNG" format either -- just
//! the classic OLE "virtual file" pair, `FileGroupDescriptorW` (an HGLOBAL
//! listing one or more virtual filenames) and `FileContents` (the bytes for
//! one of those, delivered as an `IStream`, requested per index via
//! `lindex`). Windows only allows one `IDropTarget` registered per HWND
//! (unlike macOS, where views can be stacked and each register for
//! different pasteboard types), so instead of adding a second target we
//! revoke winit's own registration and install one that handles all three
//! cases ourselves: local file paths (`CF_HDROP`, the same format winit
//! read), the registered "PNG" format (present on some sites/browsers), and
//! the `FileGroupDescriptorW`/`FileContents` virtual-file pair.

use std::path::PathBuf;

use windows::core::{implement, Ref, Result as WinResult, PCWSTR};
use windows::Win32::Foundation::{HGLOBAL, HWND, POINTL};
use windows::Win32::System::Com::{
    IDataObject, IStream, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    ReleaseStgMedium, RegisterDragDrop, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, FILEDESCRIPTORW, HDROP};
use raw_window_handle::HasWindowHandle;

use crate::app::BrowserDropQueue;

#[implement(IDropTarget)]
struct DropTarget {
    queue: BrowserDropQueue,
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data_object: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let accepts = match data_object.as_ref() {
            Some(data_object) => {
                log_available_formats(data_object);
                !read_dropped_items(data_object).is_empty()
            }
            None => {
                eprintln!("[img-ref-tool] drag: DragEnter got a null data object");
                false
            }
        };
        eprintln!("[img-ref-tool] drag: DragEnter accepts={accepts}");
        unsafe {
            *effect = if accepts { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        // Cursor feedback was already decided in `DragEnter`; nothing here
        // changes it, since which formats are present can't change mid-drag.
        unsafe {
            if *effect != DROPEFFECT_NONE {
                *effect = DROPEFFECT_COPY;
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> WinResult<()> {
        Ok(())
    }

    fn Drop(
        &self,
        data_object: Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        _effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let Some(data_object) = data_object.as_ref() else {
            return Ok(());
        };
        let items = read_dropped_items(data_object);
        eprintln!("[img-ref-tool] drag: Drop produced {} item(s)", items.len());
        if let Ok(mut queue) = self.queue.lock() {
            queue.extend(items);
        }
        Ok(())
    }
}

/// Diagnostic: prints every clipboard format the drag data object offers,
/// so we can see what a given browser/site actually places on the drag
/// (as opposed to what we currently know how to read).
fn log_available_formats(data_object: &IDataObject) {
    let Ok(enum_formats) = (unsafe { data_object.EnumFormatEtc(DATADIR_GET.0 as u32) }) else {
        eprintln!("[img-ref-tool] drag: EnumFormatEtc failed");
        return;
    };
    loop {
        let mut buffer = [FORMATETC::default()];
        let mut fetched = 0u32;
        let hr = unsafe { enum_formats.Next(&mut buffer, Some(&mut fetched)) };
        if hr.is_err() || fetched == 0 {
            break;
        }
        eprintln!(
            "[img-ref-tool] drag: offered cfFormat={} tymed={}",
            buffer[0].cfFormat, buffer[0].tymed
        );
    }
}

/// Reads whatever the data object offers as encoded image bytes: local file
/// paths (read straight off disk), the registered "PNG" format some
/// sites/browsers place for dragged images, and the OLE virtual-file pair
/// Chrome actually uses (`FileGroupDescriptorW` + `FileContents`).
fn read_dropped_items(data_object: &IDataObject) -> Vec<Vec<u8>> {
    let mut items = Vec::new();
    items.extend(read_file_paths(data_object).into_iter().filter_map(|path| std::fs::read(path).ok()));

    let png_format = registered_format("PNG");
    match read_global_format(data_object, png_format) {
        Some(png) => {
            eprintln!(
                "[img-ref-tool] drag: read {} byte(s) from PNG format {png_format}",
                png.len()
            );
            items.push(png);
        }
        None => eprintln!("[img-ref-tool] drag: no data for PNG format {png_format}"),
    }

    let virtual_files = read_virtual_files(data_object);
    eprintln!(
        "[img-ref-tool] drag: read {} virtual file(s)",
        virtual_files.len()
    );
    items.extend(virtual_files);

    items
}

fn registered_format(name: &str) -> u16 {
    let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide_name` is a valid, null-terminated UTF-16 string that
    // outlives this call.
    (unsafe { RegisterClipboardFormatW(PCWSTR(wide_name.as_ptr())) }) as u16
}

/// Reads the classic OLE "virtual file" drag pair: `FileGroupDescriptorW`
/// lists how many virtual files there are (and their names), and
/// `FileContents` gives the bytes for item `i` via an `IStream` when
/// requested with `lindex = i`.
fn read_virtual_files(data_object: &IDataObject) -> Vec<Vec<u8>> {
    let descriptor_format = registered_format("FileGroupDescriptorW");
    let Some(descriptor_bytes) = read_global_format(data_object, descriptor_format) else {
        eprintln!("[img-ref-tool] drag: no FileGroupDescriptorW ({descriptor_format})");
        return Vec::new();
    };
    if descriptor_bytes.len() < 4 {
        return Vec::new();
    }
    let count = u32::from_ne_bytes(descriptor_bytes[0..4].try_into().unwrap()) as usize;
    eprintln!("[img-ref-tool] drag: FileGroupDescriptorW lists {count} item(s)");

    let contents_format = registered_format("FileContents");
    let entry_size = std::mem::size_of::<FILEDESCRIPTORW>();
    let mut files = Vec::new();
    for index in 0..count {
        let offset = 4 + index * entry_size;
        if offset + entry_size > descriptor_bytes.len() {
            break;
        }
        // SAFETY: `offset` was just checked to leave `entry_size` bytes in
        // bounds; the struct is `packed(1)` so an unaligned read is correct.
        let descriptor = unsafe {
            (descriptor_bytes.as_ptr().add(offset) as *const FILEDESCRIPTORW).read_unaligned()
        };
        // Copy the field out by value first: `descriptor` is a packed
        // struct, so a reference straight into `cFileName` would be
        // unaligned even though we never dereference misaligned memory.
        let file_name_raw = descriptor.cFileName;
        let name_len = file_name_raw
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(file_name_raw.len());
        let file_name = String::from_utf16_lossy(&file_name_raw[..name_len]);

        match read_file_contents(data_object, contents_format, index as i32) {
            Some(bytes) => {
                eprintln!(
                    "[img-ref-tool] drag: FileContents[{index}] name={file_name:?} = {} byte(s){}",
                    bytes.len(),
                    text_preview(&bytes)
                );
                files.push(bytes);
            }
            None => eprintln!(
                "[img-ref-tool] drag: no data for FileContents[{index}] name={file_name:?}"
            ),
        }
    }
    files
}

/// Diagnostic: if a small virtual file's bytes look like plain text (e.g. an
/// Internet Shortcut / .url file, which is what some sites drag instead of
/// actual image bytes), show a preview so we can tell it apart from a real
/// (binary) image payload in the logs.
fn text_preview(bytes: &[u8]) -> String {
    if bytes.len() > 2048 {
        return String::new();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if text.chars().all(|c| !c.is_control() || c == '\n' || c == '\r') => {
            format!(" text={text:?}")
        }
        _ => String::new(),
    }
}

fn read_file_contents(data_object: &IDataObject, format: u16, index: i32) -> Option<Vec<u8>> {
    let format_etc = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: (TYMED_ISTREAM.0 as u32) | (TYMED_HGLOBAL.0 as u32),
    };
    // SAFETY: `format_etc` describes a well-formed request; `GetData` gives
    // back an owned `STGMEDIUM` that we release ourselves below.
    let mut medium = unsafe { data_object.GetData(&format_etc) }.ok()?;
    let bytes = if medium.tymed == TYMED_ISTREAM.0 as u32 {
        // SAFETY: `tymed` says the `pstm` union field is active.
        unsafe { medium.u.pstm.as_ref() }.and_then(read_stream_to_vec)
    } else if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        // SAFETY: `tymed` says the `hGlobal` union field is active.
        copy_global(unsafe { medium.u.hGlobal })
    } else {
        None
    };
    // SAFETY: `medium` came from `GetData` above and is only released once.
    unsafe { ReleaseStgMedium(&mut medium) };
    bytes
}

fn read_stream_to_vec(stream: &IStream) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let mut read = 0u32;
        // SAFETY: `buffer` is valid for `buffer.len()` bytes for the
        // duration of the call.
        let hr = unsafe {
            stream.Read(
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                Some(&mut read),
            )
        };
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
        if hr.is_err() {
            break;
        }
    }
    if bytes.is_empty() { None } else { Some(bytes) }
}

fn read_global_format(data_object: &IDataObject, format: u16) -> Option<Vec<u8>> {
    let format_etc = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: `format_etc` describes a well-formed request; `GetData` gives
    // back an owned `STGMEDIUM` that we release ourselves below.
    let mut medium = unsafe { data_object.GetData(&format_etc) }.ok()?;
    // SAFETY: we asked for `TYMED_HGLOBAL`, so `u.hGlobal` is the active
    // union field.
    let handle = unsafe { medium.u.hGlobal };
    let bytes = copy_global(handle);
    // SAFETY: `medium` came from `GetData` above and is only released once.
    unsafe { ReleaseStgMedium(&mut medium) };
    bytes
}

fn copy_global(handle: HGLOBAL) -> Option<Vec<u8>> {
    if handle.0.is_null() {
        return None;
    }
    // SAFETY: `handle` is a valid HGLOBAL for the lifetime of this function;
    // it is unlocked before returning.
    unsafe {
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(handle);
        Some(bytes)
    }
}

fn read_file_paths(data_object: &IDataObject) -> Vec<PathBuf> {
    let format_etc = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let Ok(mut medium) = (unsafe { data_object.GetData(&format_etc) }) else {
        return Vec::new();
    };
    let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
    let mut paths = Vec::new();
    // SAFETY: `hdrop` came from a successful `CF_HDROP` `GetData` above.
    unsafe {
        let count = DragQueryFileW(hdrop, 0xffff_ffff, None);
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, None) as usize;
            let mut buffer = vec![0u16; len + 1];
            DragQueryFileW(hdrop, index, Some(&mut buffer));
            paths.push(PathBuf::from(String::from_utf16_lossy(&buffer[..len])));
        }
        ReleaseStgMedium(&mut medium);
    }
    paths
}

/// Takes over drag-and-drop for the window eframe just created: revokes
/// winit's own `IDropTarget` (registered only for `CF_HDROP`) and installs
/// ours, which additionally understands browser-sourced image data.
pub fn install(cc: &eframe::CreationContext<'_>) -> Option<BrowserDropQueue> {
    let raw_window_handle::RawWindowHandle::Win32(handle) = cc.window_handle().ok()?.as_raw() else {
        return None;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);

    let queue: BrowserDropQueue = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let target: IDropTarget = DropTarget { queue: queue.clone() }.into();

    // SAFETY: `hwnd` is the live window eframe just created on this thread;
    // OLE is already initialized (winit's own drag-and-drop needed it to
    // register in the first place).
    let registered = unsafe {
        // Ignore failure: if winit didn't register one for some reason,
        // revoking is a harmless no-op.
        let _ = RevokeDragDrop(hwnd);
        RegisterDragDrop(hwnd, &target)
    };
    match registered {
        Ok(()) => eprintln!("[img-ref-tool] drag: RegisterDragDrop succeeded"),
        Err(error) => {
            eprintln!("[img-ref-tool] drag: RegisterDragDrop failed: {error}");
            return None;
        }
    }

    Some(queue)
}
