//! Raw ELF64 structures and constants (only what the loader needs).

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Ehdr {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Dyn {
    pub d_tag: i64,
    pub d_val: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Sym {
    pub st_name: u32,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: u16,
    pub st_value: u64,
    pub st_size: u64,
}

impl Sym {
    pub fn bind(&self) -> u8 {
        self.st_info >> 4
    }
    pub fn kind(&self) -> u8 {
        self.st_info & 0xf
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64,
}

pub const EM_X86_64: u16 = 62;
pub const ET_DYN: u16 = 3;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_RELRO: u32 = 0x6474_e552;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const DT_NULL: i64 = 0;
pub const DT_NEEDED: i64 = 1;
pub const DT_PLTRELSZ: i64 = 2;
pub const DT_HASH: i64 = 4;
pub const DT_STRTAB: i64 = 5;
pub const DT_SYMTAB: i64 = 6;
pub const DT_RELA: i64 = 7;
pub const DT_RELASZ: i64 = 8;
pub const DT_INIT: i64 = 12;
pub const DT_FINI: i64 = 13;
pub const DT_SONAME: i64 = 14;
pub const DT_TEXTREL: i64 = 22;
pub const DT_JMPREL: i64 = 23;
pub const DT_FLAGS: i64 = 30;
pub const DF_TEXTREL: u64 = 0x4;
pub const DT_INIT_ARRAY: i64 = 25;
pub const DT_FINI_ARRAY: i64 = 26;
pub const DT_INIT_ARRAYSZ: i64 = 27;
pub const DT_FINI_ARRAYSZ: i64 = 28;
pub const DT_RELRSZ: i64 = 35;
pub const DT_RELR: i64 = 36;
pub const DT_GNU_HASH: i64 = 0x6fff_fef5;
pub const DT_ANDROID_RELA: i64 = 0x6000_0011;
pub const DT_ANDROID_RELASZ: i64 = 0x6000_0012;
pub const DT_ANDROID_RELR: i64 = 0x6fff_e000;
pub const DT_ANDROID_RELRSZ: i64 = 0x6fff_e001;

pub const SHN_UNDEF: u16 = 0;

pub const STB_GLOBAL: u8 = 1;
pub const STB_WEAK: u8 = 2;

pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;
pub const STT_TLS: u8 = 6;

pub const EM_AARCH64: u16 = 183;

/// The machine and relocation numbers of the architecture Veneer runs on.
/// The guest library must be built for the same one: there is no emulation.
#[cfg(target_arch = "x86_64")]
pub mod arch {
    pub const MACHINE: u16 = super::EM_X86_64;
    pub const NAME: &str = "x86-64";
    pub const R_ABS64: u32 = 1; // R_X86_64_64
    pub const R_GLOB_DAT: u32 = 6;
    pub const R_JUMP_SLOT: u32 = 7;
    pub const R_RELATIVE: u32 = 8;
    pub const R_IRELATIVE: u32 = 37;
    /// DTPMOD64, DTPOFF64, TPOFF64.
    pub const R_TLS: &[u32] = &[16, 17, 18];
}

#[cfg(target_arch = "aarch64")]
pub mod arch {
    pub const MACHINE: u16 = super::EM_AARCH64;
    pub const NAME: &str = "AArch64";
    pub const R_ABS64: u32 = 257;
    pub const R_GLOB_DAT: u32 = 1025;
    pub const R_JUMP_SLOT: u32 = 1026;
    pub const R_RELATIVE: u32 = 1027;
    pub const R_IRELATIVE: u32 = 1032;
    /// TLS_DTPMOD64, TLS_DTPREL64, TLS_TPREL64, TLSDESC.
    pub const R_TLS: &[u32] = &[1028, 1029, 1030, 1031];
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("Veneer supports x86-64 and AArch64 hosts only");
