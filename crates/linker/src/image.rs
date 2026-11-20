//! Mapping, relocating and initialising a single guest ELF image.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::elf::*;
use crate::{resolve_global, stubs};

const PAGE: usize = 4096;

fn page_floor(x: usize) -> usize {
    x & !(PAGE - 1)
}
fn page_ceil(x: usize) -> usize {
    (x + PAGE - 1) & !(PAGE - 1)
}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Format(&'static str),
    Map(std::io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "I/O error: {e}"),
            LoadError::Format(m) => write!(f, "bad ELF: {m}"),
            LoadError::Map(e) => write!(f, "mmap failed: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

pub struct Image {
    pub name: String,
    pub c_name: CString,
    /// Start of the reserved address range.
    pub map_start: usize,
    pub map_size: usize,
    /// Load bias: `runtime address = bias + p_vaddr`.
    pub bias: usize,
    pub phdr: *const Phdr,
    pub phnum: usize,
    pub needed: Vec<String>,
    /// Names of imports nothing could satisfy (bound to abort thunks).
    pub unresolved: Vec<String>,
    symtab: *const Sym,
    strtab: *const c_char,
    nsyms: usize,
    exports: HashMap<String, usize>,
    init: Option<usize>,
    init_array: (usize, usize),
}

unsafe impl Send for Image {}
unsafe impl Sync for Image {}

impl Image {
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.map_start && addr < self.map_start + self.map_size
    }

    /// Address of an exported (defined) symbol.
    pub fn symbol(&self, name: &str) -> Option<usize> {
        self.exports.get(name).copied()
    }

    pub fn exports(&self) -> impl Iterator<Item = (&str, usize)> {
        self.exports.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub(crate) fn nearest_symbol_raw(&self, addr: usize) -> Option<(*const c_char, usize)> {
        let mut best: Option<(*const c_char, usize)> = None;
        for i in 1..self.nsyms {
            let s = unsafe { &*self.symtab.add(i) };
            if s.st_shndx == SHN_UNDEF || s.kind() == STT_TLS {
                continue;
            }
            let start = self.bias + s.st_value as usize;
            if start > addr {
                continue;
            }
            if s.st_size != 0 && addr >= start + s.st_size as usize {
                continue;
            }
            if best.is_none_or(|(_, b)| start > b) {
                best = Some((unsafe { self.strtab.add(s.st_name as usize) }, start));
            }
        }
        best
    }

    pub fn nearest_symbol(&self, addr: usize) -> Option<(String, usize)> {
        self.nearest_symbol_raw(addr).map(|(n, a)| {
            (unsafe { CStr::from_ptr(n) }.to_string_lossy().into_owned(), a)
        })
    }

    /// Run `DT_INIT` and the `DT_INIT_ARRAY` constructors.
    ///
    /// # Safety
    /// Executes arbitrary guest code.
    pub unsafe fn run_init(&self) {
        type InitFn = unsafe extern "C" fn(i32, *const *const c_char, *const *const c_char);
        let argv: [*const c_char; 2] = [self.c_name.as_ptr(), std::ptr::null()];
        let envp: [*const c_char; 1] = [std::ptr::null()];
        if let Some(init) = self.init {
            log::debug!("{}: DT_INIT @ {:#x}", self.name, init);
            let f: InitFn = unsafe { std::mem::transmute(init) };
            unsafe { f(1, argv.as_ptr(), envp.as_ptr()) };
        }
        let (arr, count) = self.init_array;
        log::debug!("{}: running {} constructors", self.name, count);
        for i in 0..count {
            let p = unsafe { *((arr + i * 8) as *const usize) };
            if p == 0 || p == usize::MAX {
                continue;
            }
            log::trace!("{}: ctor[{i}] @ {}+{:#x}", self.name, self.name, p - self.bias);
            let f: InitFn = unsafe { std::mem::transmute(p) };
            unsafe { f(1, argv.as_ptr(), envp.as_ptr()) };
        }
    }
}

struct Mapped {
    start: usize,
    size: usize,
    bias: usize,
}

fn prot_of(flags: u32) -> i32 {
    let mut p = 0;
    if flags & PF_R != 0 {
        p |= libc::PROT_READ;
    }
    if flags & PF_W != 0 {
        p |= libc::PROT_WRITE;
    }
    if flags & PF_X != 0 {
        p |= libc::PROT_EXEC;
    }
    p
}

fn map_segments(file: &File, phdrs: &[Phdr]) -> Result<Mapped, LoadError> {
    let loads: Vec<&Phdr> = phdrs.iter().filter(|p| p.p_type == PT_LOAD).collect();
    if loads.is_empty() {
        return Err(LoadError::Format("no PT_LOAD segments"));
    }
    let min = page_floor(loads.iter().map(|p| p.p_vaddr as usize).min().unwrap());
    let max = page_ceil(
        loads
            .iter()
            .map(|p| (p.p_vaddr + p.p_memsz) as usize)
            .max()
            .unwrap(),
    );
    let size = max - min;
    let start = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if start == libc::MAP_FAILED {
        return Err(LoadError::Map(std::io::Error::last_os_error()));
    }
    let start = start as usize;
    let bias = start - min;

    for ph in loads {
        let prot = prot_of(ph.p_flags);
        let seg_start = bias + ph.p_vaddr as usize;
        let seg_page = page_floor(seg_start);
        let file_end = seg_start + ph.p_filesz as usize;
        let mem_end = seg_start + ph.p_memsz as usize;

        if ph.p_filesz > 0 {
            let file_page = page_floor(ph.p_offset as usize);
            let len = file_end - seg_page;
            let p = unsafe {
                libc::mmap(
                    seg_page as *mut c_void,
                    len,
                    prot,
                    libc::MAP_PRIVATE | libc::MAP_FIXED,
                    file.as_raw_fd(),
                    file_page as libc::off_t,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(LoadError::Map(std::io::Error::last_os_error()));
            }
        }

        // Zero-fill the tail (.bss): first the rest of the last file page,
        // then fresh anonymous pages.
        let file_page_end = page_ceil(file_end);
        if mem_end > file_end {
            if ph.p_flags & PF_W != 0 && ph.p_filesz > 0 && file_page_end > file_end {
                unsafe {
                    std::ptr::write_bytes(file_end as *mut u8, 0, file_page_end - file_end);
                }
            }
            let anon_start = if ph.p_filesz > 0 { file_page_end } else { seg_page };
            let anon_end = page_ceil(mem_end);
            if anon_end > anon_start {
                let p = unsafe {
                    libc::mmap(
                        anon_start as *mut c_void,
                        anon_end - anon_start,
                        prot,
                        libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if p == libc::MAP_FAILED {
                    return Err(LoadError::Map(std::io::Error::last_os_error()));
                }
            }
        }
    }
    Ok(Mapped { start, size, bias })
}

/// Decoder for Android's packed relocation format ("APS2").
struct PackedRelocs<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u64,
    group_left: u64,
    group_flags: u64,
    group_offset_delta: i64,
    cur: Rela,
}

const GROUPED_BY_INFO: u64 = 1;
const GROUPED_BY_OFFSET_DELTA: u64 = 2;
const GROUPED_BY_ADDEND: u64 = 4;
const GROUP_HAS_ADDEND: u64 = 8;

impl<'a> PackedRelocs<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 || &data[..4] != b"APS2" {
            return None;
        }
        let mut s = PackedRelocs {
            data,
            pos: 4,
            remaining: 0,
            group_left: 0,
            group_flags: 0,
            group_offset_delta: 0,
            cur: Rela {
                r_offset: 0,
                r_info: 0,
                r_addend: 0,
            },
        };
        s.remaining = s.sleb() as u64;
        s.cur.r_offset = s.sleb() as u64;
        Some(s)
    }

    fn sleb(&mut self) -> i64 {
        let mut value: i64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.data[self.pos];
            self.pos += 1;
            value |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return value;
            }
        }
    }
}

impl Iterator for PackedRelocs<'_> {
    type Item = Rela;

    fn next(&mut self) -> Option<Rela> {
        if self.remaining == 0 {
            return None;
        }
        if self.group_left == 0 {
            self.group_left = self.sleb() as u64;
            self.group_flags = self.sleb() as u64;
            if self.group_flags & GROUPED_BY_OFFSET_DELTA != 0 {
                self.group_offset_delta = self.sleb();
            }
            if self.group_flags & GROUPED_BY_INFO != 0 {
                self.cur.r_info = self.sleb() as u64;
            }
            if self.group_flags & GROUP_HAS_ADDEND != 0 {
                if self.group_flags & GROUPED_BY_ADDEND != 0 {
                    self.cur.r_addend = self.cur.r_addend.wrapping_add(self.sleb());
                }
            } else {
                self.cur.r_addend = 0;
            }
        }
        if self.group_flags & GROUPED_BY_OFFSET_DELTA != 0 {
            self.cur.r_offset = self.cur.r_offset.wrapping_add(self.group_offset_delta as u64);
        } else {
            let d = self.sleb();
            self.cur.r_offset = self.cur.r_offset.wrapping_add(d as u64);
        }
        if self.group_flags & GROUPED_BY_INFO == 0 {
            self.cur.r_info = self.sleb() as u64;
        }
        if self.group_flags & GROUP_HAS_ADDEND != 0 && self.group_flags & GROUPED_BY_ADDEND == 0 {
            self.cur.r_addend = self.cur.r_addend.wrapping_add(self.sleb());
        }
        self.remaining -= 1;
        self.group_left -= 1;
        Some(self.cur)
    }
}

#[derive(Default)]
struct DynInfo {
    symtab: usize,
    strtab: usize,
    gnu_hash: usize,
    hash: usize,
    rela: usize,
    relasz: usize,
    jmprel: usize,
    pltrelsz: usize,
    android_rela: usize,
    android_relasz: usize,
    relr: usize,
    relrsz: usize,
    init: usize,
    init_array: usize,
    init_arraysz: usize,
    needed: Vec<u64>,
    soname: Option<u64>,
    textrel: bool,
}

unsafe fn gnu_hash_nsyms(table: usize) -> usize {
    unsafe {
        let hdr = table as *const u32;
        let nbuckets = *hdr as usize;
        let symoffset = *hdr.add(1) as usize;
        let bloom_size = *hdr.add(2) as usize;
        let buckets = (table + 16 + bloom_size * 8) as *const u32;
        let chain = buckets.add(nbuckets);
        let mut max = 0usize;
        for i in 0..nbuckets {
            max = max.max(*buckets.add(i) as usize);
        }
        if max < symoffset {
            return symoffset;
        }
        let mut i = max;
        while *chain.add(i - symoffset) & 1 == 0 {
            i += 1;
        }
        i + 1
    }
}

pub(crate) fn load(path: &Path) -> Result<Image, LoadError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len < size_of::<Ehdr>() {
        return Err(LoadError::Format("file too small"));
    }
    // Temporary read-only view of the file for header parsing.
    let view = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if view == libc::MAP_FAILED {
        return Err(LoadError::Map(std::io::Error::last_os_error()));
    }
    let result = load_inner(path, &file, view as usize, len);
    unsafe { libc::munmap(view, len) };
    result
}

fn load_inner(path: &Path, file: &File, view: usize, len: usize) -> Result<Image, LoadError> {
    let eh = unsafe { &*(view as *const Ehdr) };
    if &eh.e_ident[..4] != b"\x7fELF" || eh.e_ident[4] != 2 || eh.e_ident[5] != 1 {
        return Err(LoadError::Format("not a little-endian ELF64 file"));
    }
    if eh.e_machine != EM_X86_64 || eh.e_type != ET_DYN {
        return Err(LoadError::Format("not an x86-64 shared object"));
    }
    let phoff = eh.e_phoff as usize;
    let phnum = eh.e_phnum as usize;
    if phoff + phnum * size_of::<Phdr>() > len {
        return Err(LoadError::Format("program headers out of range"));
    }
    let phdrs: Vec<Phdr> =
        unsafe { std::slice::from_raw_parts((view + phoff) as *const Phdr, phnum) }.to_vec();
    if phdrs.iter().any(|p| p.p_type == PT_TLS) {
        // Android API < 29 binaries use emulated TLS; real ELF TLS would need
        // a bionic-compatible thread pointer layout.
        return Err(LoadError::Format("ELF TLS segments are not supported yet"));
    }

    // W^X: refuse images that ask for memory that is both writable and
    // executable. (Relocations only ever touch writable data here.)
    if phdrs
        .iter()
        .any(|p| p.p_type == PT_LOAD && p.p_flags & PF_W != 0 && p.p_flags & PF_X != 0)
    {
        return Err(LoadError::Format("segment is both writable and executable"));
    }

    let mapped = map_segments(file, &phdrs)?;
    let bias = mapped.bias;

    // Program headers as seen in memory (needed by the guest's unwinder).
    let phdr_mem = phdrs
        .iter()
        .find(|p| {
            p.p_type == PT_LOAD
                && eh.e_phoff >= p.p_offset
                && eh.e_phoff < p.p_offset + p.p_filesz
        })
        .map(|p| bias + (p.p_vaddr + eh.e_phoff - p.p_offset) as usize)
        .ok_or(LoadError::Format("program headers are not mapped"))?;

    let dynamic = phdrs
        .iter()
        .find(|p| p.p_type == PT_DYNAMIC)
        .ok_or(LoadError::Format("no PT_DYNAMIC"))?;

    let mut d = DynInfo::default();
    let mut p = (bias + dynamic.p_vaddr as usize) as *const Dyn;
    loop {
        let e = unsafe { *p };
        let ptr = bias + e.d_val as usize;
        let val = e.d_val as usize;
        match e.d_tag {
            DT_NULL => break,
            DT_NEEDED => d.needed.push(e.d_val),
            DT_SONAME => d.soname = Some(e.d_val),
            DT_SYMTAB => d.symtab = ptr,
            DT_STRTAB => d.strtab = ptr,
            DT_GNU_HASH => d.gnu_hash = ptr,
            DT_HASH => d.hash = ptr,
            DT_RELA => d.rela = ptr,
            DT_RELASZ => d.relasz = val,
            DT_JMPREL => d.jmprel = ptr,
            DT_PLTRELSZ => d.pltrelsz = val,
            DT_ANDROID_RELA => d.android_rela = ptr,
            DT_ANDROID_RELASZ => d.android_relasz = val,
            DT_RELR | DT_ANDROID_RELR => d.relr = ptr,
            DT_RELRSZ | DT_ANDROID_RELRSZ => d.relrsz = val,
            DT_INIT => d.init = ptr,
            DT_INIT_ARRAY => d.init_array = ptr,
            DT_INIT_ARRAYSZ => d.init_arraysz = val,
            DT_TEXTREL => d.textrel = true,
            DT_FLAGS if e.d_val & DF_TEXTREL != 0 => d.textrel = true,
            _ => {}
        }
        p = unsafe { p.add(1) };
    }
    if d.textrel {
        // Text relocations would write into read+exec code (W^X); bionic
        // refuses them too since API 23.
        return Err(LoadError::Format("text relocations are not supported (W^X)"));
    }
    if d.symtab == 0 || d.strtab == 0 {
        return Err(LoadError::Format("missing dynamic symbol table"));
    }
    let symtab = d.symtab as *const Sym;
    let strtab = d.strtab as *const c_char;
    let cstr = |off: u64| -> String {
        unsafe { CStr::from_ptr(strtab.add(off as usize)) }
            .to_string_lossy()
            .into_owned()
    };

    let nsyms = if d.gnu_hash != 0 {
        unsafe { gnu_hash_nsyms(d.gnu_hash) }
    } else if d.hash != 0 {
        unsafe { *((d.hash + 4) as *const u32) as usize }
    } else {
        return Err(LoadError::Format("no symbol hash table"));
    };

    let name = d
        .soname
        .map(&cstr)
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let mut exports = HashMap::new();
    for i in 1..nsyms {
        let s = unsafe { &*symtab.add(i) };
        if s.st_shndx == SHN_UNDEF || s.kind() == STT_TLS {
            continue;
        }
        if s.bind() != STB_GLOBAL && s.bind() != STB_WEAK {
            continue;
        }
        exports.insert(cstr(s.st_name as u64), bias + s.st_value as usize);
    }

    // --- relocation -------------------------------------------------------
    let mut unresolved: Vec<String> = Vec::new();
    let mut cache: HashMap<u32, usize> = HashMap::new();
    let mut sym_addr = |idx: u32| -> usize {
        if idx == 0 {
            return 0;
        }
        if let Some(a) = cache.get(&idx) {
            return *a;
        }
        let s = unsafe { &*symtab.add(idx as usize) };
        let addr = if s.st_shndx != SHN_UNDEF {
            bias + s.st_value as usize
        } else {
            let n = cstr(s.st_name as u64);
            match resolve_global(&n) {
                Some(a) => a,
                None if s.bind() == STB_WEAK => 0,
                None => {
                    let stub = if s.kind() == STT_OBJECT {
                        // Give data imports some zeroed backing store so
                        // reads do not fault before we notice.
                        Box::leak(vec![0u8; 256].into_boxed_slice()).as_ptr() as usize
                    } else {
                        stubs::make(&n)
                    };
                    unresolved.push(n);
                    stub
                }
            }
        };
        cache.insert(idx, addr);
        addr
    };

    let mut ifuncs: Vec<(usize, usize)> = Vec::new();
    let mut apply = |r: &Rela| -> Result<(), LoadError> {
        let ty = (r.r_info & 0xffff_ffff) as u32;
        let sym = (r.r_info >> 32) as u32;
        let target = (bias + r.r_offset as usize) as *mut usize;
        let value = match ty {
            R_X86_64_RELATIVE => bias.wrapping_add(r.r_addend as usize),
            R_X86_64_64 => sym_addr(sym).wrapping_add(r.r_addend as usize),
            R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => sym_addr(sym),
            R_X86_64_IRELATIVE => {
                ifuncs.push((target as usize, bias.wrapping_add(r.r_addend as usize)));
                return Ok(());
            }
            R_X86_64_DTPMOD64 | R_X86_64_DTPOFF64 | R_X86_64_TPOFF64 => {
                return Err(LoadError::Format("TLS relocation in image without PT_TLS"));
            }
            0 => return Ok(()),
            _ => return Err(LoadError::Format("unsupported relocation type")),
        };
        unsafe { target.write_unaligned(value) };
        Ok(())
    };

    // RELR (compact relative relocations).
    if d.relr != 0 {
        let words = unsafe { std::slice::from_raw_parts(d.relr as *const u64, d.relrsz / 8) };
        let mut next = 0usize;
        for &w in words {
            if w & 1 == 0 {
                let t = (bias + w as usize) as *mut usize;
                unsafe { *t = (*t).wrapping_add(bias) };
                next = bias + w as usize + 8;
            } else {
                let mut bits = w >> 1;
                let mut a = next;
                while bits != 0 {
                    if bits & 1 != 0 {
                        let t = a as *mut usize;
                        unsafe { *t = (*t).wrapping_add(bias) };
                    }
                    bits >>= 1;
                    a += 8;
                }
                next += 63 * 8;
            }
        }
    }
    if d.android_rela != 0 {
        let data =
            unsafe { std::slice::from_raw_parts(d.android_rela as *const u8, d.android_relasz) };
        let it = PackedRelocs::new(data).ok_or(LoadError::Format("bad packed relocations"))?;
        for r in it {
            apply(&r)?;
        }
    }
    for (tab, size) in [(d.rela, d.relasz), (d.jmprel, d.pltrelsz)] {
        if tab == 0 {
            continue;
        }
        let relas =
            unsafe { std::slice::from_raw_parts(tab as *const Rela, size / size_of::<Rela>()) };
        for r in relas {
            apply(r)?;
        }
    }
    for (target, resolver) in ifuncs {
        let f: extern "C" fn() -> usize = unsafe { std::mem::transmute(resolver) };
        unsafe { (target as *mut usize).write_unaligned(f()) };
    }

    // Make RELRO read-only, as the code may rely on (or probe) it.
    for ph in phdrs.iter().filter(|p| p.p_type == PT_GNU_RELRO) {
        let s = page_floor(bias + ph.p_vaddr as usize);
        let e = page_floor(bias + (ph.p_vaddr + ph.p_memsz) as usize);
        if e > s {
            unsafe { libc::mprotect(s as *mut c_void, e - s, libc::PROT_READ) };
        }
    }

    unresolved.sort();
    unresolved.dedup();
    Ok(Image {
        c_name: CString::new(name.clone()).unwrap(),
        name,
        map_start: mapped.start,
        map_size: mapped.size,
        bias,
        phdr: phdr_mem as *const Phdr,
        phnum,
        needed: d.needed.iter().map(|o| cstr(*o)).collect(),
        unresolved,
        symtab,
        strtab,
        nsyms,
        exports,
        init: (d.init != 0).then_some(d.init),
        init_array: (d.init_array, d.init_arraysz / 8),
    })
}
