#![windows_subsystem = "windows"]

mod apfs;
mod disk;

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::rc::Rc;

use windows_sys::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use apfs::{Entry, Kind, Volume};

const ID_COMBO: i32 = 101;
const ID_UP: i32 = 102;
const ID_ADDR: i32 = 103;
const ID_OPENIMG: i32 = 104;
const ID_REFRESH: i32 = 105;
const ID_EXTRACT: i32 = 106;
const ID_LIST: i32 = 107;
const ID_STATUS: i32 = 108;

// ------------------------------------------------------------------ helpers

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..end])
}

fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

fn fmt_time(ns: u64) -> String {
    if ns == 0 {
        return String::new();
    }
    let secs = (ns / 1_000_000_000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// Hari sejak 1970-01-01 -> (tahun, bulan, hari), algoritma Howard Hinnant.
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || (c as u32) < 32 { '_' } else { c })
        .collect()
}

unsafe fn msgbox(hwnd: HWND, text: &str, icon: u32) {
    MessageBoxW(hwnd, wide(text).as_ptr(), w!("APFS Explorer"), MB_OK | icon);
}

unsafe fn child(class: *const u16, text: &str, style: u32, ex: u32, parent: HWND, id: i32) -> HWND {
    let t = wide(text);
    CreateWindowExW(
        ex,
        class,
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        0,
        0,
        0,
        0,
        parent,
        id as usize as _,
        GetModuleHandleW(null()),
        null(),
    )
}

// -------------------------------------------------------------------- state

struct Slot {
    label: String,
    vol: Rc<Volume>,
}

struct App {
    hwnd: HWND,
    combo: HWND,
    btn_img: HWND,
    btn_refresh: HWND,
    btn_extract: HWND,
    up: HWND,
    addr: HWND,
    list: HWND,
    status: HWND,
    phys: Vec<Slot>,
    imgs: Vec<Slot>,
    cur: Option<usize>,
    path: Vec<(String, u64)>,
    entries: Vec<Entry>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// try_borrow_mut: notifikasi yang dikirim ListView saat kita mengisinya (re-entrancy) diabaikan.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn open_slots(dev: &disk::Device, prefix: &str) -> io::Result<Vec<Slot>> {
    let cont = apfs::Container::open(dev.clone())?;
    Ok(cont
        .volumes()?
        .into_iter()
        .map(|v| Slot { label: format!("{prefix} • {}", v.name), vol: Rc::new(v) })
        .collect())
}

fn extract(vol: &Volume, e: &Entry, dest: &Path) -> io::Result<u64> {
    let target = dest.join(sanitize(&e.name));
    match e.kind {
        Kind::Dir => {
            fs::create_dir_all(&target)?;
            let mut n = 0;
            for c in vol.read_dir(e.ino)? {
                n += extract(vol, &c, &target)?;
            }
            Ok(n)
        }
        Kind::File => {
            let mut w = BufWriter::new(File::create(&target)?);
            vol.read_file(e.ino, &mut w)?;
            Ok(1)
        }
        _ => Ok(0), // symlink & lainnya dilewati
    }
}

impl App {
    unsafe fn layout(&self) {
        let mut rc: RECT = zeroed();
        GetClientRect(self.hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);
        MoveWindow(self.combo, 4, 4, 330, 200, 1);
        MoveWindow(self.btn_img, 340, 4, 110, 24, 1);
        MoveWindow(self.btn_refresh, 454, 4, 80, 24, 1);
        MoveWindow(self.btn_extract, 538, 4, 120, 24, 1);
        MoveWindow(self.up, 4, 34, 40, 24, 1);
        MoveWindow(self.addr, 48, 34, (w - 52).max(50), 24, 1);
        MoveWindow(self.list, 0, 64, w, (h - 64 - 22).max(50), 1);
        MoveWindow(self.status, 6, h - 19, (w - 12).max(10), 18, 1);
    }

    unsafe fn set_status(&self, s: &str) {
        SetWindowTextW(self.status, wide(s).as_ptr());
    }

    fn slot(&self, idx: usize) -> Option<&Slot> {
        if idx < self.phys.len() {
            self.phys.get(idx)
        } else {
            self.imgs.get(idx - self.phys.len())
        }
    }

    unsafe fn rebuild_combo(&mut self, select: Option<usize>) {
        SendMessageW(self.combo, CB_RESETCONTENT, 0, 0);
        for s in self.phys.iter().chain(self.imgs.iter()) {
            SendMessageW(self.combo, CB_ADDSTRING, 0, wide(&s.label).as_ptr() as LPARAM);
        }
        let total = self.phys.len() + self.imgs.len();
        if total == 0 {
            self.cur = None;
            self.entries.clear();
            SendMessageW(self.list, LVM_DELETEALLITEMS, 0, 0);
            SetWindowTextW(self.addr, wide("").as_ptr());
            return;
        }
        SendMessageW(self.combo, CB_SETCURSEL, select.unwrap_or(0).min(total - 1), 0);
        self.select_volume();
    }

    unsafe fn refresh(&mut self) {
        self.set_status("Memindai disk fisik…");
        let mut phys = Vec::new();
        let mut notes = Vec::new();
        for n in 0..24u32 {
            let file = match disk::open_physical(n) {
                Ok(f) => Rc::new(f),
                Err(e) => {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        notes.push(format!("Disk {n}: akses ditolak (jalankan sebagai Administrator)"));
                    }
                    continue;
                }
            };
            let dev = disk::Device::new(file, 0);
            let Ok(parts) = disk::apfs_partitions(&dev) else { continue };
            for p in parts {
                match open_slots(&dev.at(p.offset), &format!("Disk {n}")) {
                    Ok(mut s) => phys.append(&mut s),
                    Err(e) => notes.push(format!("Disk {n}: {e}")),
                }
            }
        }
        self.phys = phys;
        self.rebuild_combo(None);
        let msg = if self.phys.is_empty() {
            format!("Tidak ada volume APFS ditemukan. {}", notes.join(" | "))
        } else {
            format!("{} volume APFS ditemukan. {}", self.phys.len(), notes.join(" | "))
        };
        self.set_status(&msg);
    }

    unsafe fn add_image(&mut self, path: &str) {
        let name = Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let result = (|| -> io::Result<Vec<Slot>> {
            let dev = disk::Device::new(Rc::new(disk::open_image(path)?), 0);
            let mut all = Vec::new();
            for p in disk::apfs_partitions(&dev)? {
                all.append(&mut open_slots(&dev.at(p.offset), &format!("Image {name}"))?);
            }
            Ok(all)
        })();
        match result {
            Ok(s) if !s.is_empty() => {
                self.imgs.extend(s);
                let last = self.phys.len() + self.imgs.len() - 1;
                self.rebuild_combo(Some(last));
            }
            Ok(_) => msgbox(self.hwnd, "Tidak ada partisi/volume APFS di image ini.\n\nCatatan: hanya image mentah (raw) yang didukung, bukan .dmg terkompresi (UDIF).", MB_ICONWARNING),
            Err(e) => msgbox(self.hwnd, &format!("Gagal membuka image: {e}"), MB_ICONERROR),
        }
    }

    unsafe fn select_volume(&mut self) {
        let sel = SendMessageW(self.combo, CB_GETCURSEL, 0, 0);
        if sel < 0 {
            return;
        }
        self.cur = Some(sel as usize);
        self.path = vec![(String::new(), apfs::ROOT_INO)];
        self.load_dir();
    }

    /// Memuat direktori terakhir di `path`; mengembalikan false jika gagal.
    unsafe fn load_dir(&mut self) -> bool {
        let Some(cur) = self.cur else { return false };
        let Some(vol) = self.slot(cur).map(|s| s.vol.clone()) else { return false };
        let ino = self.path.last().map(|p| p.1).unwrap_or(apfs::ROOT_INO);
        match vol.read_dir(ino) {
            Ok(mut v) => {
                v.sort_by_key(|e| (e.kind != Kind::Dir, e.name.to_lowercase()));
                self.entries = v;
                self.show_entries();
                let names: Vec<&str> = self.path[1..].iter().map(|p| p.0.as_str()).collect();
                SetWindowTextW(self.addr, wide(&format!("/{}", names.join("/"))).as_ptr());
                let label = self.slot(cur).map(|s| s.label.clone()).unwrap_or_default();
                self.set_status(&format!("{} item • {}", self.entries.len(), label));
                true
            }
            Err(e) => {
                msgbox(self.hwnd, &format!("Gagal membaca direktori: {e}"), MB_ICONERROR);
                false
            }
        }
    }

    unsafe fn show_entries(&self) {
        SendMessageW(self.list, LVM_DELETEALLITEMS, 0, 0);
        for (i, e) in self.entries.iter().enumerate() {
            let mut name = wide(&e.name);
            let mut it: LVITEMW = zeroed();
            it.mask = LVIF_TEXT | LVIF_IMAGE;
            it.iItem = i as i32;
            it.pszText = name.as_mut_ptr();
            it.iImage = icon_index(e);
            SendMessageW(self.list, LVM_INSERTITEMW, 0, &it as *const _ as LPARAM);

            let size = if e.kind == Kind::File { fmt_size(e.size) } else { String::new() };
            let kind = match e.kind {
                Kind::Dir => "Folder",
                Kind::File => "Berkas",
                Kind::Link => "Symlink",
                Kind::Other => "Lainnya",
            };
            for (col, text) in [(1, size), (2, fmt_time(e.mtime)), (3, kind.to_string())] {
                let mut t = wide(&text);
                let mut sub: LVITEMW = zeroed();
                sub.iSubItem = col;
                sub.pszText = t.as_mut_ptr();
                SendMessageW(self.list, LVM_SETITEMTEXTW, i, &sub as *const _ as LPARAM);
            }
        }
    }

    unsafe fn sel_index(&self) -> Option<usize> {
        let i = SendMessageW(self.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
        if i >= 0 && (i as usize) < self.entries.len() { Some(i as usize) } else { None }
    }

    unsafe fn open_selected(&mut self) {
        let Some(i) = self.sel_index() else { return };
        let e = self.entries[i].clone();
        if e.kind == Kind::Dir {
            self.path.push((e.name, e.ino));
            if !self.load_dir() {
                self.path.pop();
            }
        } else {
            self.set_status("Pilih berkas lalu klik “Salin ke PC…” untuk mengekstrak.");
        }
    }

    unsafe fn up(&mut self) {
        if self.path.len() > 1 {
            self.path.pop();
            self.load_dir();
        }
    }

    unsafe fn selected_entry(&self) -> Option<(HWND, Rc<Volume>, Entry)> {
        let e = self.entries.get(self.sel_index()?)?.clone();
        let vol = self.slot(self.cur?)?.vol.clone();
        Some((self.hwnd, vol, e))
    }
}

unsafe fn icon_index(e: &Entry) -> i32 {
    let mut sfi: SHFILEINFOW = zeroed();
    let (name, attr) = if e.kind == Kind::Dir {
        (wide("folder"), FILE_ATTRIBUTE_DIRECTORY)
    } else {
        (wide(&sanitize(&e.name)), FILE_ATTRIBUTE_NORMAL)
    };
    SHGetFileInfoW(
        name.as_ptr(),
        attr,
        &mut sfi,
        size_of::<SHFILEINFOW>() as u32,
        SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
    );
    sfi.iIcon
}

// ------------------------------------------------------------------ actions

unsafe fn open_image() {
    let Some(hwnd) = with_app(|a| a.hwnd) else { return };
    let mut buf = [0u16; 520];
    let filter: Vec<u16> = "Raw disk image (*.img;*.dmg;*.raw;*.iso)\0*.img;*.dmg;*.raw;*.iso\0Semua file\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut ofn: OPENFILENAMEW = zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return;
    }
    let path = from_wide(&buf);
    with_app(|a| a.add_image(&path));
}

unsafe fn extract_selected() {
    let Some(Some((hwnd, vol, e))) = with_app(|a| a.selected_entry()) else {
        if let Some(h) = with_app(|a| a.hwnd) {
            msgbox(h, "Pilih berkas atau folder terlebih dahulu.", MB_ICONINFORMATION);
        }
        return;
    };
    let mut name_buf = [0u16; 260];
    let mut bi: BROWSEINFOW = zeroed();
    bi.hwndOwner = hwnd;
    bi.pszDisplayName = name_buf.as_mut_ptr();
    bi.lpszTitle = w!("Pilih folder tujuan");
    bi.ulFlags = BIF_RETURNONLYFSDIRS;
    let pidl = SHBrowseForFolderW(&bi);
    if pidl.is_null() {
        return;
    }
    let mut path = [0u16; 520];
    let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
    CoTaskMemFree(pidl as *const _);
    if ok == 0 {
        return;
    }
    let dest = from_wide(&path);
    match extract(&vol, &e, Path::new(&dest)) {
        Ok(n) => msgbox(hwnd, &format!("Selesai: {n} berkas disalin ke\n{dest}"), MB_ICONINFORMATION),
        Err(er) => msgbox(hwnd, &format!("Gagal menyalin: {er}"), MB_ICONERROR),
    }
}

unsafe fn handle_command(id: i32, code: u32) {
    match id {
        ID_OPENIMG => open_image(),
        ID_REFRESH => {
            with_app(|a| a.refresh());
        }
        ID_UP => {
            with_app(|a| a.up());
        }
        ID_EXTRACT => extract_selected(),
        ID_COMBO if code == CBN_SELCHANGE => {
            with_app(|a| a.select_volume());
        }
        _ => {}
    }
}

// ------------------------------------------------------------------- window

unsafe fn create_controls(hwnd: HWND) -> App {
    let combo = child(w!("COMBOBOX"), "", WS_VSCROLL | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | CBS_HASSTRINGS as u32, 0, hwnd, ID_COMBO);
    let btn_img = child(w!("BUTTON"), "Buka Image…", WS_TABSTOP | BS_PUSHBUTTON as u32, 0, hwnd, ID_OPENIMG);
    let btn_refresh = child(w!("BUTTON"), "Segarkan", WS_TABSTOP | BS_PUSHBUTTON as u32, 0, hwnd, ID_REFRESH);
    let btn_extract = child(w!("BUTTON"), "Salin ke PC…", WS_TABSTOP | BS_PUSHBUTTON as u32, 0, hwnd, ID_EXTRACT);
    let up = child(w!("BUTTON"), "↑", WS_TABSTOP | BS_PUSHBUTTON as u32, 0, hwnd, ID_UP);
    let addr = child(w!("EDIT"), "", ES_READONLY as u32 | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, hwnd, ID_ADDR);
    let list = child(
        WC_LISTVIEWW,
        "",
        WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
        WS_EX_CLIENTEDGE,
        hwnd,
        ID_LIST,
    );
    let status = child(w!("STATIC"), "Siap.", 0, 0, hwnd, ID_STATUS);

    let font = GetStockObject(DEFAULT_GUI_FONT);
    for h in [combo, btn_img, btn_refresh, btn_extract, up, addr, list, status] {
        SendMessageW(h, WM_SETFONT, font as usize, 1);
    }

    SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);

    // Ikon sistem (folder/berkas) sebagai image list ListView.
    let mut sfi: SHFILEINFOW = zeroed();
    let himl = SHGetFileInfoW(
        w!("x.txt"),
        FILE_ATTRIBUTE_NORMAL,
        &mut sfi,
        size_of::<SHFILEINFOW>() as u32,
        SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
    );
    SendMessageW(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, himl as isize);

    for (i, (title, width)) in [("Nama", 300), ("Ukuran", 90), ("Diubah (UTC)", 130), ("Jenis", 80)].iter().enumerate() {
        let mut txt = wide(title);
        let mut col: LVCOLUMNW = zeroed();
        col.mask = LVCF_TEXT | LVCF_WIDTH;
        col.cx = *width;
        col.pszText = txt.as_mut_ptr();
        SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
    }

    App {
        hwnd,
        combo,
        btn_img,
        btn_refresh,
        btn_extract,
        up,
        addr,
        list,
        status,
        phys: Vec::new(),
        imgs: Vec::new(),
        cur: None,
        path: Vec::new(),
        entries: Vec::new(),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let app = create_controls(hwnd);
            APP.with(|a| *a.borrow_mut() = Some(app));
            with_app(|a| a.layout());
            // Pindai disk setelah jendela tampil, supaya UI tidak terasa macet saat start.
            PostMessageW(hwnd, WM_APP, 0, 0);
            0
        }
        WM_APP => {
            with_app(|a| a.refresh());
            0
        }
        WM_SIZE => {
            with_app(|a| a.layout());
            0
        }
        WM_COMMAND => {
            handle_command((wp & 0xFFFF) as i32, ((wp >> 16) & 0xFFFF) as u32);
            0
        }
        WM_NOTIFY => {
            let h = &*(lp as *const NMHDR);
            if h.code == NM_DBLCLK as u32 {
                with_app(|a| {
                    if h.hwndFrom == a.list {
                        a.open_selected();
                    }
                });
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let hinst = GetModuleHandleW(null());
        let class = w!("ApfsExplorerWnd");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class,
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            0,
            class,
            w!("APFS Explorer"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            980,
            620,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
