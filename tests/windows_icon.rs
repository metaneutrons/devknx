// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

const LOAD_LIBRARY_AS_DATAFILE: u32 = 0x0000_0002;
const RT_GROUP_ICON: usize = 14;
const APP_ICON_ID: usize = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn FindResourceW(module: *mut c_void, name: *const u16, kind: *const u16) -> *mut c_void;
    fn SizeofResource(module: *mut c_void, resource: *mut c_void) -> u32;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

#[test]
fn executable_embeds_app_icon() {
    let executable = Path::new(env!("CARGO_BIN_EXE_devknx"));
    let wide_path: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Load only the PE resources; the test must not start a second GUI process.
    let module = unsafe {
        LoadLibraryExW(
            wide_path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE,
        )
    };
    assert!(
        !module.is_null(),
        "could not load devknx PE resources: {}",
        std::io::Error::last_os_error()
    );

    // winres::set_icon installs the application icon under numeric ID 1.
    let icon = unsafe {
        FindResourceW(
            module,
            APP_ICON_ID as *const u16,
            RT_GROUP_ICON as *const u16,
        )
    };
    assert!(!icon.is_null(), "devknx.exe has no application icon group");
    assert!(unsafe { SizeofResource(module, icon) } >= 20);

    // Counter-probe: the same lookup must reject an icon ID not in the binary.
    let absent = unsafe {
        FindResourceW(
            module,
            0x7fff_usize as *const u16,
            RT_GROUP_ICON as *const u16,
        )
    };
    assert!(absent.is_null(), "missing icon ID unexpectedly resolved");

    assert_ne!(unsafe { FreeLibrary(module) }, 0);
}
