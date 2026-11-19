//! stdio: bionic's legacy `__sF` standard streams.
//!
//! Code built against old NDK headers refers to stdin/stdout/stderr as
//! `&__sF[0..3]`, an array of bionic `FILE` structs (152 bytes each on LP64).
//! We hand out a dummy array and translate those addresses to the host
//! streams in every function that accepts a `FILE*`.

use std::collections::HashMap;
use std::ffi::{VaList, c_char, c_int, c_long, c_void};

use libc::{FILE, off_t, size_t, wchar_t};

const BIONIC_FILE_SIZE: usize = 152;

#[repr(C, align(16))]
struct FakeFiles([u8; BIONIC_FILE_SIZE * 3]);
static mut SF: FakeFiles = FakeFiles([0; BIONIC_FILE_SIZE * 3]);

type WInt = u32;

unsafe extern "C" {
    static stdin: *mut FILE;
    static stdout: *mut FILE;
    static stderr: *mut FILE;
    fn vfprintf(f: *mut FILE, fmt: *const c_char, ap: VaList) -> c_int;
    fn vfscanf(f: *mut FILE, fmt: *const c_char, ap: VaList) -> c_int;
    fn fputwc(wc: wchar_t, f: *mut FILE) -> WInt;
    fn getwc(f: *mut FILE) -> WInt;
    fn ungetwc(wc: WInt, f: *mut FILE) -> WInt;
    fn getc(f: *mut FILE) -> c_int;
}

/// Translate a guest `FILE*` into a host one.
pub unsafe fn fp(f: *mut FILE) -> *mut FILE {
    let base = &raw const SF as usize;
    let a = f as usize;
    if a >= base && a < base + BIONIC_FILE_SIZE * 3 {
        unsafe {
            match (a - base) / BIONIC_FILE_SIZE {
                0 => stdin,
                1 => stdout,
                _ => stderr,
            }
        }
    } else {
        f
    }
}

unsafe extern "C" fn b_clearerr(f: *mut FILE) {
    unsafe { libc::clearerr(fp(f)) }
}
unsafe extern "C" fn b_fclose(f: *mut FILE) -> c_int {
    unsafe { libc::fclose(fp(f)) }
}
unsafe extern "C" fn b_feof(f: *mut FILE) -> c_int {
    unsafe { libc::feof(fp(f)) }
}
unsafe extern "C" fn b_ferror(f: *mut FILE) -> c_int {
    unsafe { libc::ferror(fp(f)) }
}
unsafe extern "C" fn b_fflush(f: *mut FILE) -> c_int {
    unsafe { libc::fflush(fp(f)) }
}
unsafe extern "C" fn b_fgets(s: *mut c_char, n: c_int, f: *mut FILE) -> *mut c_char {
    unsafe { libc::fgets(s, n, fp(f)) }
}
unsafe extern "C" fn b_fileno(f: *mut FILE) -> c_int {
    unsafe { libc::fileno(fp(f)) }
}
unsafe extern "C" fn b_fprintf(f: *mut FILE, fmt: *const c_char, ap: ...) -> c_int {
    unsafe { vfprintf(fp(f), fmt, ap) }
}
unsafe extern "C" fn b_vfprintf(f: *mut FILE, fmt: *const c_char, ap: VaList) -> c_int {
    unsafe { vfprintf(fp(f), fmt, ap) }
}
unsafe extern "C" fn b_fscanf(f: *mut FILE, fmt: *const c_char, ap: ...) -> c_int {
    unsafe { vfscanf(fp(f), fmt, ap) }
}
unsafe extern "C" fn b_fputc(c: c_int, f: *mut FILE) -> c_int {
    unsafe { libc::fputc(c, fp(f)) }
}
unsafe extern "C" fn b_fputs(s: *const c_char, f: *mut FILE) -> c_int {
    unsafe { libc::fputs(s, fp(f)) }
}
unsafe extern "C" fn b_fputwc(wc: wchar_t, f: *mut FILE) -> WInt {
    unsafe { fputwc(wc, fp(f)) }
}
unsafe extern "C" fn b_fread(p: *mut c_void, size: size_t, n: size_t, f: *mut FILE) -> size_t {
    unsafe { libc::fread(p, size, n, fp(f)) }
}
unsafe extern "C" fn b_fread_chk(
    p: *mut c_void,
    size: size_t,
    n: size_t,
    f: *mut FILE,
    _buf_size: size_t,
) -> size_t {
    unsafe { libc::fread(p, size, n, fp(f)) }
}
unsafe extern "C" fn b_fwrite(p: *const c_void, size: size_t, n: size_t, f: *mut FILE) -> size_t {
    unsafe { libc::fwrite(p, size, n, fp(f)) }
}
unsafe extern "C" fn b_fwrite_chk(
    p: *const c_void,
    size: size_t,
    n: size_t,
    f: *mut FILE,
    _buf_size: size_t,
) -> size_t {
    unsafe { libc::fwrite(p, size, n, fp(f)) }
}
unsafe extern "C" fn b_fseek(f: *mut FILE, off: c_long, whence: c_int) -> c_int {
    unsafe { libc::fseek(fp(f), off, whence) }
}
unsafe extern "C" fn b_fseeko(f: *mut FILE, off: off_t, whence: c_int) -> c_int {
    unsafe { libc::fseeko(fp(f), off, whence) }
}
unsafe extern "C" fn b_ftell(f: *mut FILE) -> c_long {
    unsafe { libc::ftell(fp(f)) }
}
unsafe extern "C" fn b_ftello(f: *mut FILE) -> off_t {
    unsafe { libc::ftello(fp(f)) }
}
unsafe extern "C" fn b_getc(f: *mut FILE) -> c_int {
    unsafe { getc(fp(f)) }
}
unsafe extern "C" fn b_getwc(f: *mut FILE) -> WInt {
    unsafe { getwc(fp(f)) }
}
unsafe extern "C" fn b_setvbuf(f: *mut FILE, buf: *mut c_char, mode: c_int, size: size_t) -> c_int {
    unsafe { libc::setvbuf(fp(f), buf, mode, size) }
}
unsafe extern "C" fn b_ungetc(c: c_int, f: *mut FILE) -> c_int {
    unsafe { libc::ungetc(c, fp(f)) }
}
unsafe extern "C" fn b_ungetwc(c: WInt, f: *mut FILE) -> WInt {
    unsafe { ungetwc(c, fp(f)) }
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("__sF", &raw const SF as usize);
    m.insert("clearerr", b_clearerr as usize);
    m.insert("fclose", b_fclose as usize);
    m.insert("feof", b_feof as usize);
    m.insert("ferror", b_ferror as usize);
    m.insert("fflush", b_fflush as usize);
    m.insert("fgets", b_fgets as usize);
    m.insert("fileno", b_fileno as usize);
    m.insert("fprintf", b_fprintf as usize);
    m.insert("vfprintf", b_vfprintf as usize);
    m.insert("fscanf", b_fscanf as usize);
    m.insert("fputc", b_fputc as usize);
    m.insert("putc", b_fputc as usize);
    m.insert("fputs", b_fputs as usize);
    m.insert("fputwc", b_fputwc as usize);
    m.insert("fread", b_fread as usize);
    m.insert("__fread_chk", b_fread_chk as usize);
    m.insert("fwrite", b_fwrite as usize);
    m.insert("__fwrite_chk", b_fwrite_chk as usize);
    m.insert("fseek", b_fseek as usize);
    m.insert("fseeko", b_fseeko as usize);
    m.insert("ftell", b_ftell as usize);
    m.insert("ftello", b_ftello as usize);
    m.insert("getc", b_getc as usize);
    m.insert("fgetc", b_getc as usize);
    m.insert("getwc", b_getwc as usize);
    m.insert("setvbuf", b_setvbuf as usize);
    m.insert("ungetc", b_ungetc as usize);
    m.insert("ungetwc", b_ungetwc as usize);
}
