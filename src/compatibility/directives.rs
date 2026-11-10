//! ELF -> COFF section and directive mapping.
//!
//! | ELF section | COFF section | COFF flags |
//! |---|---|---|
//! | `.text`, `.text.X` | `.text`, `.text$X` | code (`xr`) |
//! | `.data`, `.data.X` | `.data`, `.data$X` | writable data (`dw`) |
//! | `.bss`, `.bss.X` | `.bss`, `.bss$X` | uninitialized (`bw`) |
//! | `.rodata`, `.rodata.X`, `.data.rel.ro*` | `.rdata`, `.rdata$X` | read-only data (`dr`) |
//! | `.note*`, `.comment`, `.debug*`, non-allocated sections | dropped | |
//! | `.init_array`, `.fini_array`, `.ctors`, `.dtors`, `.tdata`, `.tbss`, `.eh_frame`, ... | **error** | |
//!
//! `name$suffix` is the PE "grouped section" convention: the linker sorts
//! `.text$X` into `.text`, like ELF linkers merge `.text.X` into `.text`.
//!
//! Directives without a COFF meaning are dropped: `.type` and `.size` (ELF
//! symbol attributes), `.local`, `.file`/`.ident`/`.cfi_*`. Global
//! functions get a COFF `.def`/`.scl`/`.type 32`/`.endef` block instead.

use crate::ir::SectionKind;

/// A COFF section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoffSection {
    pub name: String,
    /// GAS COFF section flags (`x` code, `d` data, `r` read-only,
    /// `w` writable, `b` uninitialized).
    pub flags: &'static str,
}

impl CoffSection {
    /// The section switch directive.
    pub fn directive(&self) -> String {
        match self.name.as_str() {
            ".text" | ".data" | ".bss" => self.name.clone(),
            _ => format!(".section {},\"{}\"", self.name, self.flags),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionMapping {
    Keep(CoffSection),
    /// Section without runtime meaning on Windows (with the reason).
    Drop(&'static str),
}

/// ELF sections that carry runtime semantics Windows/PE handles differently.
const UNSUPPORTED: &[(&str, &str)] = &[
    (".init_array", "ELF constructors (`.init_array`) are not supported"),
    (".fini_array", "ELF destructors (`.fini_array`) are not supported"),
    (".preinit_array", "`.preinit_array` is not supported"),
    (".ctors", "ELF constructors (`.ctors`) are not supported"),
    (".dtors", "ELF destructors (`.dtors`) are not supported"),
    (".tdata", "ELF thread-local storage (`.tdata`) is not supported"),
    (".tbss", "ELF thread-local storage (`.tbss`) is not supported"),
    (".eh_frame", "ELF unwind tables (`.eh_frame`) are not supported; remove them or assemble with `.cfi_*` only"),
    (".gcc_except_table", "C++ exception tables are not supported"),
    (".got", "the global offset table cannot be written by hand on Windows"),
    (".plt", "the procedure linkage table cannot be written by hand on Windows"),
];

fn has_prefix(name: &str, prefix: &str) -> bool {
    name == prefix || name.starts_with(&format!("{prefix}."))
}

/// `.text.hot` -> `.text$hot` (with `base` as the COFF base name).
fn grouped(name: &str, elf_prefix: &str, base: &str) -> String {
    match name
        .strip_prefix(elf_prefix)
        .and_then(|r| r.strip_prefix('.'))
    {
        Some(suffix) if !suffix.is_empty() => format!("{base}${suffix}"),
        _ => base.to_string(),
    }
}

/// Maps an ELF section to its COFF counterpart.
pub fn map_section(
    name: &str,
    kind: SectionKind,
    flags: Option<&str>,
) -> Result<SectionMapping, String> {
    if let Some((_, msg)) = UNSUPPORTED.iter().find(|(p, _)| has_prefix(name, p)) {
        return Err((*msg).to_string());
    }
    let keep = |name: String, flags| Ok(SectionMapping::Keep(CoffSection { name, flags }));
    if has_prefix(name, ".data.rel.ro") {
        return keep(grouped(name, ".data.rel.ro", ".rdata"), "dr");
    }
    match kind {
        SectionKind::Text if has_prefix(name, ".text") => {
            keep(grouped(name, ".text", ".text"), "xr")
        }
        SectionKind::Data if has_prefix(name, ".data") => {
            keep(grouped(name, ".data", ".data"), "dw")
        }
        SectionKind::Bss if has_prefix(name, ".bss") => keep(grouped(name, ".bss", ".bss"), "bw"),
        SectionKind::ReadOnly if has_prefix(name, ".rodata") => {
            keep(grouped(name, ".rodata", ".rdata"), "dr")
        }
        SectionKind::Note => Ok(SectionMapping::Drop("ELF note section")),
        _ if name == ".comment" => Ok(SectionMapping::Drop("ELF comment section")),
        _ if name.starts_with(".debug") => Ok(SectionMapping::Drop("ELF debug information")),
        // Custom names: keep the name, translate the kind.
        SectionKind::Text => keep(name.to_string(), "xr"),
        SectionKind::Data => keep(name.to_string(), "dw"),
        SectionKind::Bss => keep(name.to_string(), "bw"),
        SectionKind::ReadOnly => keep(name.to_string(), "dr"),
        SectionKind::Other => match flags {
            // Non-allocated sections are never loaded.
            None | Some("") => Ok(SectionMapping::Drop("non-allocated section")),
            Some(_) => Err(format!(
                "cannot map ELF section `{name}` with flags {flags:?} to COFF"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(name: &str, kind: SectionKind) -> (String, &'static str) {
        match map_section(name, kind, None).unwrap() {
            SectionMapping::Keep(c) => (c.name, c.flags),
            other => panic!("{name}: {other:?}"),
        }
    }

    #[test]
    fn standard_sections() {
        assert_eq!(kept(".text", SectionKind::Text), (".text".into(), "xr"));
        assert_eq!(kept(".data", SectionKind::Data), (".data".into(), "dw"));
        assert_eq!(kept(".bss", SectionKind::Bss), (".bss".into(), "bw"));
        assert_eq!(
            kept(".rodata", SectionKind::ReadOnly),
            (".rdata".into(), "dr")
        );
        assert_eq!(
            kept(".rodata.str1.1", SectionKind::ReadOnly),
            (".rdata$str1.1".into(), "dr")
        );
        assert_eq!(
            kept(".text.startup", SectionKind::Text),
            (".text$startup".into(), "xr")
        );
        assert_eq!(
            kept(".data.rel.ro.local", SectionKind::Data),
            (".rdata$local".into(), "dr")
        );
        assert_eq!(kept("mycode", SectionKind::Text), ("mycode".into(), "xr"));
    }

    #[test]
    fn dropped_and_rejected() {
        assert!(matches!(
            map_section(".note.GNU-stack", SectionKind::Note, Some("")),
            Ok(SectionMapping::Drop(_))
        ));
        assert!(matches!(
            map_section(".comment", SectionKind::Other, None),
            Ok(SectionMapping::Drop(_))
        ));
        assert!(map_section(".init_array", SectionKind::Other, Some("aw"))
            .unwrap_err()
            .contains("constructors"));
        assert!(map_section(".tbss", SectionKind::Bss, Some("awT"))
            .unwrap_err()
            .contains("thread-local"));
    }

    #[test]
    fn directive_text() {
        let c = CoffSection {
            name: ".rdata".into(),
            flags: "dr",
        };
        assert_eq!(c.directive(), ".section .rdata,\"dr\"");
        let c = CoffSection {
            name: ".text".into(),
            flags: "xr",
        };
        assert_eq!(c.directive(), ".text");
    }
}
