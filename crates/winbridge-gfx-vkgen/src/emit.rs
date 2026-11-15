// SPDX-License-Identifier: GPL-3.0-or-later

//! Code emission. Output 1 (`vk`) goes to `winbridge-gfx-vk/src/generated.rs`, output 2
//! (`shims`) to `winbridge-gfx-icd/src/shims.rs`.

use crate::registry::{Decl, Registry, Struct, TyKind};
use crate::subset::{CHAIN_ALLOW, FLUSH_AFTER, HOST_HOOKS, ICD_CUSTOM, SYNC};
use std::collections::BTreeSet;
use std::fmt::Write;

pub struct Output {
    pub vk: String,
    pub shims: String,
}

/// `pNext` -> `p_next`, `maxImageDimension2D` -> `max_image_dimension2_d` (ash's convention).
pub fn snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if let Some(p) = prev {
                if p.is_ascii_lowercase() || p.is_ascii_digit() {
                    out.push('_');
                }
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
        prev = Some(c);
    }
    match out.as_str() {
        "type" => "ty".into(),
        _ => out,
    }
}

/// Array lengths the registry states as a formula; the Rust expression over the struct `v`.
fn derived_count(strukt: &str, member: &str) -> Option<&'static str> {
    match (strukt, member) {
        ("VkPipelineMultisampleStateCreateInfo", "pSampleMask") => Some("((v.rasterization_samples.as_raw() as usize + 31) / 32)"),
        ("VkShaderModuleCreateInfo", "pCode") => Some("(v.code_size / 4)"),
        _ => None,
    }
}

fn rs_scalar(ty: &str) -> String {
    match ty {
        "uint8_t" => "u8".into(),
        "uint16_t" => "u16".into(),
        "uint32_t" => "u32".into(),
        "uint64_t" => "u64".into(),
        "int32_t" => "i32".into(),
        "int" => "i32".into(),
        "float" => "f32".into(),
        "size_t" => "usize".into(),
        "char" => "std::ffi::c_char".into(),
        "void" => "std::ffi::c_void".into(),
        // ash names a flag-bits enum after its flags type: VkSampleCountFlagBits -> vk::SampleCountFlags.
        t if t.starts_with("Vk") => format!("vk::{}", t[2..].replace("FlagBits", "Flags")),
        t => panic!("no Rust mapping for C type {t}"),
    }
}


#[derive(Clone)]
enum Elem {
    Pod(String),
    Handle { ty: String, disp: bool },
    Struct { ty: String, plain: bool },
}

#[derive(Clone)]
enum PKind {
    Handle { ty: String, disp: bool },
    Value,
    StructIn { ty: String },
    StrIn,
    OutStruct { ty: String, header: bool },
    OutHandle { ty: String, disp: bool },
    /// Count of an enumeration pair; `arr` indexes the array parameter.
    EnumCount { arr: usize },
    EnumArray { elem: Elem },
    /// `const T*` read as `count` elements, `count` being a by-value sibling parameter.
    ArrayIn { elem: Elem, count: String },
    OutScalar { ty: String },
    /// Created handles written to an array of `count` (a Rust expression over the parameters).
    OutHandleArray { ty: String, disp: bool, count: String },
    Skip,
}

#[derive(Clone)]
struct P {
    rs: String,
    rs_ty: String,
    kind: PKind,
}

struct Cmd {
    name: String,
    ret_result: bool,
    params: Vec<P>,
    /// `fp_v1_N` table selector and how to find the dispatch object.
    core: (u32, u32),
}

struct Gen<'a> {
    reg: &'a Registry,
    names: Vec<String>,
    cmds: Vec<Cmd>,
    bodies: BTreeSet<String>,
    in_roots: BTreeSet<String>,
    out_roots: BTreeSet<String>,
}

/// Prints, for every core 1.0-1.3 command the generator cannot yet handle, why not.
pub fn survey(reg: &Registry) {
    let names: Vec<String> =
        reg.commands.iter().filter(|(_, c)| c.core.is_some_and(|v| v <= (1, 3))).map(|(n, _)| n.clone()).collect();
    let (mut ok, mut bad) = (0, 0);
    std::panic::set_hook(Box::new(|_| {}));
    for n in &names {
        let r = std::panic::catch_unwind(|| {
            let mut g = Gen::new(reg, Vec::new());
            g.classify(&[n.as_str()]);
            g.collect_structs();
        });
        match r {
            Ok(()) => ok += 1,
            Err(e) => {
                bad += 1;
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                println!("{n}: {msg}");
            }
        }
    }
    println!("survey: {ok} supported, {bad} not, of {}", names.len());
}

/// Core 1.0-1.3 commands the generator can handle, sorted by name (the wire command id is the index).
fn supported(reg: &Registry) -> Vec<String> {
    let names: Vec<String> =
        reg.commands.iter().filter(|(_, c)| c.core.is_some_and(|v| v <= (1, 3))).map(|(n, _)| n.clone()).collect();
    let prev = std::panic::take_hook();
    if std::env::var_os("VKGEN_DEBUG").is_none() { std::panic::set_hook(Box::new(|_| {})); }
    let ok = names
        .into_iter()
        .filter(|n| {
            std::panic::catch_unwind(|| {
                let mut g = Gen::new(reg, Vec::new());
                g.classify(&[n.as_str()]);
                g.collect_structs();
                let mut sink = String::new();
                for b in g.bodies.clone() {
                    g.emit_struct_codecs(&mut sink, &b);
                }
                for c in &g.cmds {
                    g.emit_guest(&mut sink, c);
                    g.emit_host(&mut sink, c);
                }
            })
            .is_ok()
        })
        .collect();
    std::panic::set_hook(prev);
    ok
}

pub fn generate(reg: &Registry) -> Output {
    let mut g = Gen::new(reg, supported(reg));
    g.classify_all();
    g.collect_structs();
    Output { vk: g.emit_vk(), shims: g.emit_shims() }
}

impl<'a> Gen<'a> {
    fn param_rs_ty(&self, p: &Decl) -> String {
        let t = rs_scalar(&p.ty);
        match p.ptr {
            0 => t,
            1 => format!("*{} {}", if p.base_const { "const" } else { "mut" }, t),
            n => panic!("parameter {}: {n} pointer levels unsupported", p.name),
        }
    }

    fn new(reg: &'a Registry, names: Vec<String>) -> Self {
        Gen { reg, names, cmds: Vec::new(), bodies: BTreeSet::new(), in_roots: BTreeSet::new(), out_roots: BTreeSet::new() }
    }

    fn classify_all(&mut self) {
        let names: Vec<String> = self.names.clone();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.classify(&refs);
    }

    fn classify(&mut self, names: &[&str]) {
        for &name in names {
            let c = self.reg.commands.get(name).unwrap_or_else(|| panic!("{name} not in registry"));
            let ret_result = match c.ret.as_str() {
                "VkResult" => true,
                "void" => false,
                r => panic!("{name}: return type {r} unsupported"),
            };
            // Pair every enumeration count with its array.
            let mut kinds: Vec<Option<PKind>> = vec![None; c.params.len()];
            for (i, p) in c.params.iter().enumerate() {
                if let Some(len) = &p.len {
                    if let Some(ci) = c.params.iter().position(|q| &q.name == len && q.ptr == 1 && !q.base_const && q.ty == "uint32_t") {
                        let elem = match (self.reg.kind(&p.ty), p.ty.as_str()) {
                            (TyKind::Handle { dispatchable }, _) => Elem::Handle { ty: p.ty.clone(), disp: dispatchable },
                            (TyKind::Struct, t) if self.reg.is_pod(t) && !self.reg.structs[t].has_header() => Elem::Pod(rs_scalar(t)),
                            (TyKind::Struct, t) if self.reg.is_pod(t) && self.reg.structs[t].has_header() => Elem::Struct { ty: t.to_string(), plain: true },
                            (TyKind::Scalar, t) => Elem::Pod(rs_scalar(t)),
                            _ => panic!("{name}: enumeration element {} unsupported", p.ty),
                        };
                        kinds[ci] = Some(PKind::EnumCount { arr: i });
                        kinds[i] = Some(PKind::EnumArray { elem });
                    }
                }
            }
            let mut params = Vec::new();
            for (i, p) in c.params.iter().enumerate() {
                if p.is_array {
                    panic!("{name}: array parameter {} is not supported by the generator", p.name);
                }
                let kind = if let Some(k) = kinds[i].clone() {
                    k
                } else if p.ty == "VkAllocationCallbacks" {
                    PKind::Skip
                } else {
                    let tk = self.reg.kind(&p.ty);
                    match (p.ptr, &tk, p.base_const) {
                        (0, TyKind::Handle { dispatchable }, _) => PKind::Handle { ty: p.ty.clone(), disp: *dispatchable },
                        (0, TyKind::Scalar, _) => PKind::Value,
                        (1, _, true) if p.ty == "char" && p.len.is_none() => PKind::StrIn,
                        (1, TyKind::Struct | TyKind::Union, true) if p.len.is_none() => PKind::StructIn { ty: p.ty.clone() },
                        (1, TyKind::Scalar, false) if p.len.is_none() => PKind::OutScalar { ty: rs_scalar(&p.ty) },
                        (1, TyKind::Handle { dispatchable }, false) if p.len.as_ref().is_some_and(|l| l.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '>')) => {
                            let l = p.len.as_ref().unwrap();
                            let count = match l.split_once("->") {
                                Some((a, b)) => format!("(*{}).{}", snake(a), snake(b)),
                                None => snake(l),
                            };
                            PKind::OutHandleArray { ty: p.ty.clone(), disp: *dispatchable, count }
                        }
                        (1, k, true) if p.len.as_ref().is_some_and(|l| c.params.iter().any(|q| &q.name == l && q.ptr == 0 && matches!(self.reg.kind(&q.ty), TyKind::Scalar))) => {
                            let elem = match k {
                                TyKind::Handle { dispatchable } => Elem::Handle { ty: p.ty.clone(), disp: *dispatchable },
                                TyKind::Scalar => Elem::Pod(rs_scalar(&p.ty)),
                                TyKind::Struct => Elem::Struct { ty: p.ty.clone(), plain: self.reg.is_pod(&p.ty) && !self.reg.structs[&p.ty].has_header() },
                                _ => panic!("{name}: array element {} unsupported", p.ty),
                            };
                            PKind::ArrayIn { elem, count: p.len.clone().unwrap() }
                        }
                        (1, TyKind::Struct, false) if p.len.is_none() => {
                            PKind::OutStruct { ty: p.ty.clone(), header: self.reg.structs[&p.ty].has_header() }
                        }
                        (1, TyKind::Handle { dispatchable }, false) if p.len.is_none() => {
                            PKind::OutHandle { ty: p.ty.clone(), disp: *dispatchable }
                        }
                        _ => panic!("{name}: parameter {} ({}, {} ptr) is not supported by the generator", p.name, p.ty, p.ptr),
                    }
                };
                params.push(P { rs: snake(&p.name), rs_ty: self.param_rs_ty(p), kind });
            }
            let core = c.core.unwrap_or_else(|| panic!("{name} is not a core command"));
            self.cmds.push(Cmd { name: name.to_string(), ret_result, params, core });
        }
    }

    fn reach(&mut self, name: &str, as_root: bool) {
        let s = &self.reg.structs[name];
        if as_root && s.has_header() {
            self.in_roots.insert(name.to_string());
        }
        if !self.bodies.insert(name.to_string()) {
            return;
        }
        let members: Vec<Decl> = s.members.clone();
        for m in members {
            if m.name == "pNext" || m.name == "sType" {
                continue;
            }
            if matches!(self.reg.kind(&m.ty), TyKind::Struct | TyKind::Union) {
                self.reach(&m.ty, true);
            }
        }
    }

    fn collect_structs(&mut self) {
        let mut roots_in = Vec::new();
        let mut roots_out = Vec::new();
        for c in &self.cmds {
            for p in &c.params {
                match &p.kind {
                    PKind::StructIn { ty } | PKind::ArrayIn { elem: Elem::Struct { ty, plain: false }, .. } => roots_in.push(ty.clone()),
                    PKind::OutStruct { ty, header: true } => roots_out.push(ty.clone()),
                    _ => {}
                }
            }
        }
        for r in roots_in {
            self.reach(&r, true);
        }
        self.out_roots.extend(roots_out);
        loop {
            let before = (self.bodies.len(), self.in_roots.len());
            for r in self.in_roots.clone() {
                for a in self.chain_members(&r) {
                    self.reach(&a, false);
                }
            }
            if before == (self.bodies.len(), self.in_roots.len()) {
                break;
            }
        }
        for r in &self.out_roots {
            for a in self.chain_members(r) {
                assert!(self.reg.is_pod(&a), "{a} extends the output root {r} but is not plain data");
            }
        }
    }

    fn chain_members(&self, root: &str) -> Vec<String> {
        CHAIN_ALLOW
            .iter()
            .copied()
            .filter(|a| {
                self.reg.structs.get(*a).unwrap_or_else(|| panic!("CHAIN_ALLOW: {a} not in registry")).extends.iter().any(|e| e == root)
            })
            .map(|a| a.to_string())
            .collect()
    }

    // ---------------------------------------------------------------- struct codecs

    fn emit_struct_codecs(&self, o: &mut String, name: &str) {
        let s = &self.reg.structs[name];
        let ty = format!("vk::{}", &name[2..]);
        let hdr = s.has_header();
        let pod = self.reg.is_pod(name);
        let suffix = if hdr { "_body" } else { "" };
        if hdr {
            writeln!(
                o,
                "const _: () = assert!(std::mem::offset_of!({ty}, s_type) == 0 && std::mem::offset_of!({ty}, p_next) == 8);"
            )
            .unwrap();
        }
        let (mut enc, mut dec) = (String::new(), String::new());
        if pod {
            if hdr {
                writeln!(enc, "e.pod_slice((v as *const {ty} as *const u8).add(16), std::mem::size_of::<{ty}>() - 16);").unwrap();
                writeln!(dec, "d.pod_slice_into((out as *mut u8).add(16), std::mem::size_of::<{ty}>() - 16)?;").unwrap();
            } else {
                writeln!(enc, "e.pod(v);").unwrap();
                writeln!(dec, "d.pod_into(out)?;").unwrap();
            }
        } else {
            for (i, m) in s.members.iter().enumerate() {
                if hdr && i < 2 {
                    continue;
                }
                let (e, d) = self.member_codec(s, m);
                enc.push_str(&e);
                dec.push_str(&d);
            }
        }
        let chk: Vec<String> = s
            .members
            .iter()
            .filter(|m| m.ptr > 0 && m.len.is_some() && m.name != "pNext")
            .map(|m| format!("let mut chk_{} = None::<usize>;", snake(&m.name)))
            .collect();
        let chk_end: Vec<String> = s
            .members
            .iter()
            .filter(|m| m.ptr > 0 && m.len.as_ref().is_some_and(|l| !l.starts_with("latexmath")) && m.name != "pNext" && !pod)
            .map(|m| {
                format!(
                    "if let Some(n) = chk_{f} {{ if (*out).{l} as usize != n {{ return Err(CodecError::Invalid(\"array length differs from its count\")); }} }}\n",
                    f = snake(&m.name),
                    l = snake(m.len.as_ref().unwrap())
                )
            })
            .collect();
        let set_stype = if hdr { format!("(*out).s_type = {};\n", self.reg.stype(s)) } else { String::new() };
        writeln!(o, "#[cfg(feature = \"guest\")]\n{}unsafe fn enc_{name_}{suffix}(e: &mut Enc, v: &{ty}) -> Result<(), CodecError> {{\n{enc}Ok(())\n}}",
            if hdr { "" } else { "pub " }, name_ = &name[2..]).unwrap();
        writeln!(o, "#[cfg(feature = \"host\")]\n{}unsafe fn dec_{name_}{suffix}(d: &mut Dec, a: &mut Arena, s: &dyn Resolver, out: *mut {ty}) -> Result<(), CodecError> {{\n{chk}\n{dec}{chk_end}{set_stype}Ok(())\n}}",
            if hdr { "" } else { "pub " }, name_ = &name[2..], chk = chk.join("\n"), chk_end = chk_end.concat()).unwrap();
        if hdr && self.in_roots.contains(name) {
            writeln!(o, "#[cfg(feature = \"guest\")]\npub unsafe fn enc_{n}(e: &mut Enc, v: &{ty}) -> Result<(), CodecError> {{\nenc_chain_in_{n}(e, v.p_next as *const std::ffi::c_void)?;\nenc_{n}_body(e, v)\n}}", n = &name[2..]).unwrap();
            writeln!(o, "#[cfg(feature = \"host\")]\npub unsafe fn dec_{n}(d: &mut Dec, a: &mut Arena, s: &dyn Resolver, out: *mut {ty}) -> Result<(), CodecError> {{\n(*out).p_next = dec_chain_in_{n}(d, a, s)? as _;\ndec_{n}_body(d, a, s, out)\n}}", n = &name[2..]).unwrap();
        }
    }

    /// Encoder and decoder statements for one member of a non-plain struct.
    fn member_codec(&self, s: &Struct, m: &Decl) -> (String, String) {
        let f = snake(&m.name);
        let ctx = format!("{}.{}", s.name, m.name);
        let kind = self.reg.kind(&m.ty);
        let mut e = String::new();
        let mut d = String::new();
        let fail = |what: &str| -> ! { panic!("{ctx}: {what} is not supported by the generator") };
        if m.name == "pNext" {
            fail("pNext handled elsewhere");
        }
        if m.bitfield {
            fail("bitfield");
        }
        let hid = |disp: bool, expr: String| if disp { format!("crate::hid_disp({expr})") } else { expr };
        match (m.ptr, m.is_array) {
            (0, true) => match &kind {
                TyKind::Scalar => {
                    writeln!(e, "e.pod(&v.{f});").unwrap();
                    writeln!(d, "d.pod_into(&mut (*out).{f})?;").unwrap();
                }
                TyKind::Struct | TyKind::Union if self.reg.is_pod(&m.ty) => {
                    writeln!(e, "e.pod(&v.{f});").unwrap();
                    writeln!(d, "d.pod_into(&mut (*out).{f})?;").unwrap();
                }
                _ => fail("array of non-plain elements"),
            },
            (0, false) => match &kind {
                TyKind::Scalar => {
                    writeln!(e, "e.pod(&v.{f});").unwrap();
                    writeln!(d, "d.pod_into(&mut (*out).{f})?;").unwrap();
                }
                TyKind::Handle { dispatchable } => {
                    let t = format!("vk::{}", &m.ty[2..]);
                    writeln!(e, "e.u64({});", hid(*dispatchable, format!("v.{f}.as_raw()"))).unwrap();
                    writeln!(d, "(*out).{f} = <{t} as vk::Handle>::from_raw(s.resolve(d.u64()?, \"{}\")?);", m.ty).unwrap();
                }
                TyKind::Struct | TyKind::Union => {
                    let n = &m.ty[2..];
                    if self.reg.is_pod(&m.ty) && !self.reg.structs[&m.ty].has_header() {
                        writeln!(e, "e.pod(&v.{f});").unwrap();
                        writeln!(d, "d.pod_into(&mut (*out).{f})?;").unwrap();
                    } else {
                        writeln!(e, "enc_{n}(e, &v.{f})?;").unwrap();
                        writeln!(d, "dec_{n}(d, a, s, &mut (*out).{f})?;").unwrap();
                    }
                }
                TyKind::FuncPtr => fail("function pointer member"),
            },
            (1, false) if m.ty == "char" => {
                if m.len.is_some() {
                    fail("char pointer with a length");
                }
                writeln!(e, "if v.{f}.is_null() {{ e.bool(false); }} else {{ e.bool(true); e.blob(std::ffi::CStr::from_ptr(v.{f}).to_bytes()); }}").unwrap();
                writeln!(d, "if d.bool()? {{ (*out).{f} = a.cstr(d.blob()?) as _; }}").unwrap();
            }
            (1, false) => {
                let t = rs_scalar(&m.ty);
                let n = &m.ty[2..];
                match &m.len {
                    None => {
                        let TyKind::Struct = kind else { fail("single pointer to a non-struct") };
                        let plain = self.reg.is_pod(&m.ty) && !self.reg.structs[&m.ty].has_header();
                        let (enc_call, dec_call) = if plain {
                            (format!("e.pod(&*v.{f});"), "d.pod_into(p)?;".to_string())
                        } else {
                            (format!("enc_{n}(e, &*v.{f})?;"), format!("dec_{n}(d, a, s, p)?;"))
                        };
                        writeln!(e, "if v.{f}.is_null() {{ e.bool(false); }} else {{ e.bool(true); {enc_call} }}").unwrap();
                        writeln!(d, "if d.bool()? {{ let p = a.alloc::<{t}>(1); {dec_call} (*out).{f} = p as _; }}").unwrap();
                    }
                    Some(l) => {
                        let derived = if l.starts_with("latexmath") { derived_count(&s.name, &m.name) } else { None };
                        if derived.is_none() && !s.members.iter().any(|x| &x.name == l) {
                            fail("array length that is not a sibling member");
                        }
                        let (min, enc_el, dec_el): (String, String, String) = match &kind {
                            TyKind::Scalar => (
                                format!("std::mem::size_of::<{t}>()"),
                                "e.pod_slice(p, n);".into(),
                                "d.pod_slice_into(p, n)?;".into(),
                            ),
                            TyKind::Handle { dispatchable } => (
                                "8".into(),
                                format!("for i in 0..n {{ e.u64({}); }}", hid(*dispatchable, "(*p.add(i)).as_raw()".into())),
                                format!("for i in 0..n {{ *p.add(i) = <{t} as vk::Handle>::from_raw(s.resolve(d.u64()?, \"{}\")?); }}", m.ty),
                            ),
                            TyKind::Struct | TyKind::Union if self.reg.is_pod(&m.ty) && !self.reg.structs[&m.ty].has_header() => (
                                format!("std::mem::size_of::<{t}>()"),
                                "e.pod_slice(p, n);".into(),
                                "d.pod_slice_into(p, n)?;".into(),
                            ),
                            TyKind::Struct => (
                                "1".into(),
                                format!("for i in 0..n {{ enc_{n}(e, &*p.add(i))?; }}"),
                                format!("for i in 0..n {{ dec_{n}(d, a, s, p.add(i))?; }}"),
                            ),
                            _ => fail("array element type"),
                        };
                        let lf = snake(l);
                        let cnt = if let Some(expr) = derived {
                            expr.to_string()
                        } else {
                            format!("v.{lf} as usize")
                        };
                        writeln!(e, "{{ let p = v.{f}; let n = {cnt}; e.bool(!p.is_null()); if !p.is_null() {{ e.u32(n as u32); {enc_el} }} }}").unwrap();
                        writeln!(d, "if d.bool()? {{ let n = d.count({min})?; let p = a.alloc::<{t}>(n); {dec_el} (*out).{f} = p as _; chk_{f} = Some(n); }}").unwrap();
                    }
                }
            }
            (2, false) if m.ty == "char" && m.null_terminated => {
                let Some(l) = &m.len else { fail("string array without a length") };
                let lf = snake(l);
                writeln!(e, "{{ let p = v.{f}; let n = v.{lf} as usize; e.bool(!p.is_null()); if !p.is_null() {{ e.u32(n as u32); for i in 0..n {{ e.blob(std::ffi::CStr::from_ptr(*p.add(i)).to_bytes()); }} }} }}").unwrap();
                writeln!(d, "if d.bool()? {{ let n = d.count(4)?; let p = a.alloc::<*const std::ffi::c_char>(n); for i in 0..n {{ *p.add(i) = a.cstr(d.blob()?); }} (*out).{f} = p as _; chk_{f} = Some(n); }}").unwrap();
            }
            _ => fail("this pointer shape"),
        }
        (e, d)
    }

    // ---------------------------------------------------------------- chains

    fn emit_chain_functions(&self, o: &mut String, root: &str, input: bool) {
        let members = self.chain_members(root);
        let r = &root[2..];
        if input {
            let mut enc = String::new();
            let mut dec = String::new();
            for a in &members {
                let s = &self.reg.structs[a];
                let t = format!("vk::{}", &a[2..]);
                let st = self.reg.stype(s);
                writeln!(enc, "if st == {st} {{ e.u32({st}.as_raw() as u32); enc_{n}_body(e, &*(p as *const {t}))?; }} else", n = &a[2..]).unwrap();
                writeln!(dec, "t if t == {st}.as_raw() as u32 => {{ let n = a.alloc::<{t}>(1); dec_{an}_body(d, a, s, n)?; *link = n.cast(); link = &mut (*n).p_next as *mut _ as *mut *mut std::ffi::c_void; }}", an = &a[2..]).unwrap();
            }
            writeln!(o, "#[cfg(feature = \"guest\")]\nunsafe fn enc_chain_in_{r}(e: &mut Enc, mut p: *const std::ffi::c_void) -> Result<(), CodecError> {{\nwhile !p.is_null() {{\nlet base = p as *const vk::BaseInStructure;\nlet st = (*base).s_type;\n{enc}{{ /* not forwarded: dropped, as the specification allows for unknown structures */ }}\np = (*base).p_next as *const std::ffi::c_void;\n}}\ne.u32(CHAIN_END);\nOk(())\n}}").unwrap();
            writeln!(o, "#[cfg(feature = \"host\")]\nunsafe fn dec_chain_in_{r}(d: &mut Dec, a: &mut Arena, s: &dyn Resolver) -> Result<*const std::ffi::c_void, CodecError> {{\nlet mut head: *mut std::ffi::c_void = std::ptr::null_mut();\nlet mut link: *mut *mut std::ffi::c_void = &mut head;\nloop {{\nmatch d.u32()? {{\nCHAIN_END => break,\n{dec}t => return Err(CodecError::BadTag(t)),\n}}\n}}\nOk(head as *const std::ffi::c_void)\n}}").unwrap();
        } else {
            let (mut req, mut alloc, mut rep_h, mut rep_g) = (String::new(), String::new(), String::new(), String::new());
            for a in &members {
                let s = &self.reg.structs[a];
                let t = format!("vk::{}", &a[2..]);
                let st = self.reg.stype(s);
                writeln!(req, "if st == {st} {{ e.u32({st}.as_raw() as u32); }}").unwrap();
                writeln!(alloc, "t if t == {st}.as_raw() as u32 => {{ let n = a.alloc::<{t}>(1); (*n).s_type = {st}; *link = n.cast(); link = &mut (*n).p_next as *mut _ as *mut *mut std::ffi::c_void; }}").unwrap();
                writeln!(rep_h, "if st == {st} {{ e.pod_slice((p as *const u8).add(16), std::mem::size_of::<{t}>() - 16); }}").unwrap();
                writeln!(rep_g, "if st == {st} {{ d.pod_slice_into((p as *mut u8).add(16), std::mem::size_of::<{t}>() - 16)?; }}").unwrap();
            }
            writeln!(o, "#[cfg(feature = \"guest\")]\nunsafe fn enc_chain_out_req_{r}(e: &mut Enc, mut p: *const std::ffi::c_void) -> Result<(), CodecError> {{\nwhile !p.is_null() {{\nlet base = p as *const vk::BaseInStructure;\nlet st = (*base).s_type;\n{req}p = (*base).p_next as *const std::ffi::c_void;\n}}\ne.u32(CHAIN_END);\nOk(())\n}}").unwrap();
            writeln!(o, "#[cfg(feature = \"host\")]\nunsafe fn dec_chain_out_req_{r}(d: &mut Dec, a: &mut Arena) -> Result<*mut std::ffi::c_void, CodecError> {{\nlet mut head: *mut std::ffi::c_void = std::ptr::null_mut();\nlet mut link: *mut *mut std::ffi::c_void = &mut head;\nloop {{\nmatch d.u32()? {{\nCHAIN_END => break,\n{alloc}t => return Err(CodecError::BadTag(t)),\n}}\n}}\nOk(head)\n}}").unwrap();
            writeln!(o, "#[cfg(feature = \"host\")]\nunsafe fn enc_chain_out_rep_{r}(e: &mut Enc, mut p: *const std::ffi::c_void) -> Result<(), CodecError> {{\nwhile !p.is_null() {{\nlet base = p as *const vk::BaseInStructure;\nlet st = (*base).s_type;\n{rep_h}p = (*base).p_next as *const std::ffi::c_void;\n}}\nOk(())\n}}").unwrap();
            writeln!(o, "#[cfg(feature = \"guest\")]\nunsafe fn dec_chain_out_rep_{r}(d: &mut Dec, mut p: *mut std::ffi::c_void) -> Result<(), CodecError> {{\nwhile !p.is_null() {{\nlet base = p as *const vk::BaseInStructure;\nlet st = (*base).s_type;\n{rep_g}p = (*base).p_next as *mut std::ffi::c_void;\n}}\nOk(())\n}}").unwrap();
        }
    }

    // ---------------------------------------------------------------- commands

    fn first_handle(c: &Cmd) -> Option<(&P, &str)> {
        match c.params.first() {
            Some(p) => match &p.kind {
                PKind::Handle { ty, .. } => Some((p, ty.as_str())),
                _ => None,
            },
            None => None,
        }
    }

    fn level(c: &Cmd) -> &'static str {
        match Self::first_handle(c).map(|x| x.1) {
            Some("VkInstance") | Some("VkPhysicalDevice") => "Instance",
            Some("VkDevice") | Some("VkQueue") | Some("VkCommandBuffer") => "Device",
            _ => "Global",
        }
    }

    fn guest_sig(c: &Cmd) -> String {
        c.params.iter().filter(|p| !matches!(p.kind, PKind::Skip)).map(|p| format!("{}: {}", p.rs, p.rs_ty)).collect::<Vec<_>>().join(", ")
    }

    fn emit_guest(&self, o: &mut String, c: &Cmd) {
        let up = snake(&c.name).to_uppercase();
        let fname = snake(&c.name);
        let ret = if c.ret_result { "vk::Result" } else { "()" };
        let mut req = String::new();
        let mut rep = String::new();
        for p in &c.params {
            let n = &p.rs;
            match &p.kind {
                PKind::Handle { disp, .. } => {
                    let expr = if *disp { format!("crate::hid_disp({n}.as_raw())") } else { format!("{n}.as_raw()") };
                    writeln!(req, "e.u64({expr});").unwrap();
                }
                PKind::Value => writeln!(req, "e.pod(&{n});").unwrap(),
                PKind::StructIn { ty } => {
                    writeln!(req, "if {n}.is_null() {{ e.bool(false); }} else {{ e.bool(true); enc_{}(&mut e, &*{n})?; }}", &ty[2..]).unwrap()
                }
                PKind::StrIn => {
                    writeln!(req, "if {n}.is_null() {{ e.bool(false); }} else {{ e.bool(true); e.blob(std::ffi::CStr::from_ptr({n}).to_bytes()); }}").unwrap()
                }
                PKind::OutStruct { ty, header } => {
                    if *header {
                        writeln!(req, "enc_chain_out_req_{}(&mut e, (*{n}).p_next as *const std::ffi::c_void)?;", &ty[2..]).unwrap();
                        let t = format!("vk::{}", &ty[2..]);
                        writeln!(rep, "if ok {{ d.pod_slice_into(({n} as *mut u8).add(16), std::mem::size_of::<{t}>() - 16)?; dec_chain_out_rep_{}(&mut d, (*{n}).p_next as *mut std::ffi::c_void)?; }}", &ty[2..]).unwrap();
                    } else {
                        writeln!(rep, "if ok {{ d.pod_into({n})?; }}").unwrap();
                    }
                }
                PKind::OutHandle { ty, disp } => {
                    writeln!(req, "let ({n}_id, {n}_raw) = link.new_handle({disp}, \"{ty}\"); e.u64({n}_id);").unwrap();
                    writeln!(rep, "if ok {{ *{n} = <vk::{} as vk::Handle>::from_raw({n}_raw); }} else {{ link.discard_handle({n}_raw, {disp}); }}", &ty[2..]).unwrap();
                }
                PKind::EnumCount { arr } => {
                    let a = &c.params[*arr];
                    let PKind::EnumArray { elem } = &a.kind else { unreachable!() };
                    let an = &a.rs;
                    writeln!(req, "let enum_cap = if !{an}.is_null() {{ *{n} }} else {{ 0 }}; let enum_has = !{an}.is_null();\nif enum_cap as usize > MAX_ENUM {{ return Err(CodecError::BadLength(enum_cap as u64).into()); }}\ne.bool(enum_has); e.u32(enum_cap);").unwrap();
                    match elem {
                        Elem::Pod(_) => {
                            writeln!(rep, "let enum_count = d.u32()?; *{n} = enum_count; let enum_n = if enum_has && ok {{ (enum_count as usize).min(enum_cap as usize) }} else {{ 0 }};\nd.pod_slice_into({an}, enum_n)?;").unwrap();
                        }
                        Elem::Handle { ty, disp } => {
                            writeln!(req, "let mut enum_ids = Vec::with_capacity(enum_cap as usize); for _ in 0..enum_cap {{ let h = link.new_handle({disp}, \"{ty}\"); e.u64(h.0); enum_ids.push(h); }}").unwrap();
                            writeln!(rep, "let enum_count = d.u32()?; *{n} = enum_count; let enum_n = if enum_has && ok {{ (enum_count as usize).min(enum_cap as usize) }} else {{ 0 }};\nfor (i, h) in enum_ids.iter().enumerate() {{ if i < enum_n {{ *{an}.add(i) = <vk::{} as vk::Handle>::from_raw(h.1); }} else {{ link.discard_handle(h.1, {disp}); }} }}", &ty[2..]).unwrap();
                        }
                        Elem::Struct { ty, .. } => {
                            writeln!(rep, "let enum_count = d.u32()?; *{n} = enum_count; let enum_n = if enum_has && ok {{ (enum_count as usize).min(enum_cap as usize) }} else {{ 0 }};\nfor i in 0..enum_n {{ d.pod_slice_into(({an}.add(i) as *mut u8).add(16), std::mem::size_of::<vk::{}>() - 16)?; }}", &ty[2..]).unwrap();
                        }
                    }
                }
                PKind::ArrayIn { elem, count } => {
                    let cn = snake(count);
                    let body = match elem {
                        Elem::Pod(_) | Elem::Struct { plain: true, .. } => format!("e.pod_slice({n}, {cn} as usize);"),
                        Elem::Handle { disp, .. } => format!("for i in 0..{cn} as usize {{ e.u64({}); }}", if *disp { format!("crate::hid_disp((*{n}.add(i)).as_raw())") } else { format!("(*{n}.add(i)).as_raw()") }),
                        Elem::Struct { ty, plain: false } => format!("for i in 0..{cn} as usize {{ enc_{}(&mut e, &*{n}.add(i))?; }}", &ty[2..]),
                    };
                    writeln!(req, "if {n}.is_null() {{ e.bool(false); }} else {{ e.bool(true); {body} }}").unwrap();
                }
                PKind::OutScalar { .. } => {
                    writeln!(rep, "if ok {{ d.pod_into({n})?; }}").unwrap();
                }
                PKind::OutHandleArray { ty, disp, count } => {
                    writeln!(req, "let {n}_hs: Vec<(u64, u64)> = (0..({count}) as usize).map(|_| link.new_handle({disp}, \"{ty}\")).collect(); for h in &{n}_hs {{ e.u64(h.0); }}").unwrap();
                    writeln!(rep, "for (i, h) in {n}_hs.iter().enumerate() {{ if ok {{ *{n}.add(i) = <vk::{} as vk::Handle>::from_raw(h.1); }} else {{ link.discard_handle(h.1, {disp}); }} }}", &ty[2..]).unwrap();
                }
                PKind::EnumArray { .. } | PKind::Skip => {}
            }
        }
        let is_async = !SYNC.contains(&c.name.as_str())
            && !c.params.iter().any(|p| matches!(p.kind, PKind::OutStruct { .. } | PKind::OutScalar { .. } | PKind::EnumCount { .. }));
        let res_decode = if c.ret_result { "let res = vk::Result::from_raw(d.i32()?); let ok = res.as_raw() >= 0;" } else { "let ok = true;" };
        let ok_ret = if c.ret_result { "Ok(res)" } else { "Ok(())" };
        let fail = if c.ret_result { "vk::Result::ERROR_DEVICE_LOST" } else { "()" };
        writeln!(o, "#[cfg(feature = \"guest\")]\npub const CMD_{up}: u16 = {};", self.names.iter().position(|x| *x == c.name).unwrap_or(0)).unwrap();
        let flush = if FLUSH_AFTER.contains(&c.name.as_str()) { "link.flush()?;\n" } else { "" };
        let transport = if is_async {
            let res = if c.ret_result { "let res = vk::Result::SUCCESS;" } else { "" };
            format!("link.send_async(e.as_slice())?;\n{flush}{res}let ok = true;\nlet mut d = Dec::new(&[]);\n")
        } else {
            format!("let rep = link.call(e.as_slice())?;\nlet mut d = Dec::new(&rep);\n{res_decode}\n")
        };
        writeln!(o, "#[cfg(feature = \"guest\")]\npub unsafe fn {fname}(link: &mut dyn Link, {sig}) -> {ret} {{\nlet r: Result<{ret}, GuestError> = (|| {{\nlet mut e = Enc::new();\ne.u16(CMD_{up});\n{req}{transport}{rep}d.finish()?;\n{ok_ret}\n}})();\nmatch r {{ Ok(v) => v, Err(err) => {{ link.failed(\"{name}\", &err); {fail} }} }}\n}}",
            sig = Self::guest_sig(c), name = c.name).unwrap();
    }

    fn emit_host(&self, o: &mut String, c: &Cmd) {
        let fname = snake(&c.name);
        let mut dec = String::new();
        let mut args = Vec::new();
        let first = Self::first_handle(c);
        let first_id = first.map(|(p, _)| format!("{}_id", p.rs)).unwrap_or_else(|| "0".into());
        for p in &c.params {
            let n = &p.rs;
            match &p.kind {
                PKind::Handle { ty, .. } => {
                    writeln!(dec, "let {n}_id = d.u64()?; let {n} = <vk::{} as vk::Handle>::from_raw(s.resolve({n}_id, \"{ty}\")?);", &ty[2..]).unwrap();
                    args.push(n.clone());
                }
                PKind::Value => {
                    writeln!(dec, "let mut {n}: {} = std::mem::zeroed(); d.pod_into(&mut {n})?;", p.rs_ty).unwrap();
                    args.push(n.clone());
                }
                PKind::StructIn { ty } => {
                    let t = format!("vk::{}", &ty[2..]);
                    writeln!(dec, "let {n}: *const {t} = if d.bool()? {{ let p = a.alloc::<{t}>(1); dec_{}(d, a, &*s, p)?; p }} else {{ std::ptr::null() }};", &ty[2..]).unwrap();
                    args.push(n.clone());
                }
                PKind::StrIn => {
                    writeln!(dec, "let {n}: *const std::ffi::c_char = if d.bool()? {{ a.cstr(d.blob()?) }} else {{ std::ptr::null() }};").unwrap();
                    args.push(n.clone());
                }
                PKind::OutStruct { ty, header } => {
                    let t = format!("vk::{}", &ty[2..]);
                    if *header {
                        let st = self.reg.stype(&self.reg.structs[ty]);
                        writeln!(dec, "let {n} = a.alloc::<{t}>(1); (*{n}).s_type = {st}; (*{n}).p_next = dec_chain_out_req_{}(d, a)? as _;", &ty[2..]).unwrap();
                    } else {
                        writeln!(dec, "let {n} = a.alloc::<{t}>(1);").unwrap();
                    }
                    args.push(n.clone());
                }
                PKind::OutHandle { ty, .. } => {
                    writeln!(dec, "let {n}_id = d.u64()?; let mut {n} = <vk::{} as vk::Handle>::from_raw(0);", &ty[2..]).unwrap();
                    args.push(format!("&mut {n}"));
                }
                PKind::EnumCount { arr } => {
                    let a = &c.params[*arr];
                    let PKind::EnumArray { elem } = &a.kind else { unreachable!() };
                    let an = &a.rs;
                    let et = match elem {
                        Elem::Pod(t) => t.clone(),
                        Elem::Handle { ty, .. } | Elem::Struct { ty, .. } => format!("vk::{}", &ty[2..]),
                    };
                    writeln!(dec, "let enum_has = d.bool()?; let enum_cap = d.u32()?; if enum_cap as usize > MAX_ENUM {{ return Err(CodecError::BadLength(enum_cap as u64)); }}\nlet mut {n}_val: u32 = if enum_has {{ enum_cap }} else {{ 0 }};\nlet {an}: *mut {et} = if enum_has {{ a.alloc::<{et}>(enum_cap as usize) }} else {{ std::ptr::null_mut() }};").unwrap();
                    if let Elem::Struct { ty, .. } = elem {
                        writeln!(dec, "if enum_has {{ for i in 0..enum_cap as usize {{ (*{an}.add(i)).s_type = {}; }} }}", self.reg.stype(&self.reg.structs[ty])).unwrap();
                    }
                    if let Elem::Handle { .. } = elem {
                        writeln!(dec, "let mut enum_ids = Vec::new(); if enum_has {{ for _ in 0..enum_cap {{ enum_ids.push(d.u64()?); }} }}").unwrap();
                    }
                    args.push(format!("&mut {n}_val"));
                }
                PKind::ArrayIn { elem, count } => {
                    let (cn, t) = (snake(count), match elem {
                        Elem::Pod(t) => t.clone(),
                        Elem::Handle { ty, .. } => format!("vk::{}", &ty[2..]),
                        Elem::Struct { ty, .. } => format!("vk::{}", &ty[2..]),
                    });
                    let body = match elem {
                        Elem::Pod(_) => "d.pod_slice_into(p, n)?;".to_string(),
                        Elem::Handle { ty, .. } => format!("for i in 0..n {{ let id = d.u64()?; {n}_ids.push(id); *p.add(i) = <{t} as vk::Handle>::from_raw(s.resolve(id, \"{ty}\")?); }}"),
                        Elem::Struct { ty, plain: true } => "d.pod_slice_into(p, n)?;".to_string(),
                        Elem::Struct { ty, plain: false } => format!("for i in 0..n {{ dec_{}(d, a, &*s, p.add(i))?; }}", &ty[2..]),
                    };
                    writeln!(dec, "let mut {n}_ids: Vec<u64> = Vec::new();\nlet {n}: *const {t} = if d.bool()? {{ let n = {cn} as usize; if n > MAX_ENUM {{ return Err(CodecError::BadLength(n as u64)); }} let p = a.alloc::<{t}>(n); {body} p }} else {{ std::ptr::null() }};").unwrap();
                    args.push(n.clone());
                }
                PKind::OutScalar { ty } => {
                    writeln!(dec, "let mut {n}: {ty} = std::mem::zeroed();").unwrap();
                    args.push(format!("&mut {n}"));
                }
                PKind::OutHandleArray { ty, count, .. } => {
                    writeln!(dec, "let cnt = ({count}) as usize; if cnt > MAX_ENUM {{ return Err(CodecError::BadLength(cnt as u64)); }}\nlet mut {n}_ids: Vec<u64> = Vec::with_capacity(cnt); for _ in 0..cnt {{ {n}_ids.push(d.u64()?); }}\nlet {n}: *mut vk::{} = a.alloc::<vk::{}>(cnt);", &ty[2..], &ty[2..]).unwrap();
                    args.push(n.clone());
                }
                PKind::EnumArray { .. } => args.push(n.clone()),
                PKind::Skip => args.push("std::ptr::null()".into()),
            }
        }
        let call = self.host_call(c, &args);
        let (call_stmt, ok_stmt) = if c.ret_result {
            (format!("let res: vk::Result = {call};\nr.i32(res.as_raw());\nlet ok = res.as_raw() >= 0;"), "")
        } else {
            (format!("{call};"), "let ok = true;")
        };
        let destroy = if c.name.starts_with("vkDestroy") || c.name.starts_with("vkFree") {
            let arr = c.params.iter().find_map(|p| match &p.kind {
                PKind::ArrayIn { elem: Elem::Handle { .. }, .. } => Some(p.rs.clone()),
                _ => None,
            });
            if let Some(a) = arr {
                format!("for id in &{a}_ids {{ s.unregister(*id); }}\n")
            } else {
                let obj = c.params.iter().rev().find_map(|p| match &p.kind {
                    PKind::Handle { ty, .. } => Some(p.rs.clone()),
                    _ => None,
                });
                format!("s.unregister({}_id);\n", obj.unwrap())
            }
        } else {
            String::new()
        };
        // Reply order: result, then per parameter in order. Struct bodies and handle
        // registration only when ok; the enumeration count always.
        let mut reply = String::new();
        for p in &c.params {
            match &p.kind {
                PKind::OutStruct { ty, header } => {
                    let n = &p.rs;
                    let t = format!("vk::{}", &ty[2..]);
                    if *header {
                        writeln!(reply, "if ok {{ r.pod_slice(({n} as *const u8).add(16), std::mem::size_of::<{t}>() - 16); enc_chain_out_rep_{}(r, (*{n}).p_next as *const std::ffi::c_void)?; }}", &ty[2..]).unwrap();
                    } else {
                        writeln!(reply, "if ok {{ r.pod(&*{n}); }}").unwrap();
                    }
                }
                PKind::OutScalar { .. } => {
                    let n = &p.rs;
                    writeln!(reply, "if ok {{ r.pod(&{n}); }}").unwrap();
                }
                PKind::OutHandleArray { ty, count, .. } => {
                    let n = &p.rs;
                    writeln!(reply, "if ok {{ for i in 0..({count}) as usize {{ s.register({n}_ids[i], vk::Handle::as_raw(*{n}.add(i)), \"{ty}\", {first_id})?; }} }}").unwrap();
                }
                PKind::OutHandle { ty, .. } => {
                    let n = &p.rs;
                    writeln!(reply, "if ok {{ s.register({n}_id, vk::Handle::as_raw({n}), \"{ty}\", {first_id})?; }}").unwrap();
                }
                PKind::EnumCount { arr } => {
                    let a = &c.params[*arr];
                    let PKind::EnumArray { elem } = &a.kind else { unreachable!() };
                    let (n, an) = (&p.rs, &a.rs);
                    writeln!(reply, "r.u32({n}_val);").unwrap();
                    match elem {
                        Elem::Pod(_) => writeln!(reply, "if enum_has && ok {{ r.pod_slice({an}, ({n}_val as usize).min(enum_cap as usize)); }}").unwrap(),
                        Elem::Struct { ty, .. } => writeln!(reply, "if enum_has && ok {{ for i in 0..({n}_val as usize).min(enum_cap as usize) {{ r.pod_slice(({an}.add(i) as *const u8).add(16), std::mem::size_of::<vk::{}>() - 16); }} }}", &ty[2..]).unwrap(),
                        Elem::Handle { ty, .. } => writeln!(
                            reply,
                            "if enum_has && ok {{ for i in 0..({n}_val as usize).min(enum_cap as usize) {{ s.register(enum_ids[i], vk::Handle::as_raw(*{an}.add(i)), \"{ty}\", {first_id})?; }} }}"
                        )
                        .unwrap(),
                    }
                }
                _ => {}
            }
        }
        writeln!(o, "#[cfg(feature = \"host\")]\nunsafe fn exec_{fname}<H: Host>(s: &mut H, d: &mut Dec, a: &mut Arena, r: &mut Enc) -> Result<(), CodecError> {{\n{dec}{call_stmt}\n{ok_stmt}\n{reply}{destroy}Ok(())\n}}").unwrap();
    }

    fn host_call(&self, c: &Cmd, args: &[String]) -> String {
        let a = args.join(", ");
        if HOST_HOOKS.contains(&c.name.as_str()) {
            // Hooks take the same arguments minus the allocation callbacks.
            let kept: Vec<&String> = c.params.iter().zip(args).filter(|(p, _)| !matches!(p.kind, PKind::Skip)).map(|(_, a)| a).collect();
            let kept = kept.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            return format!("s.{}({kept})", snake(&c.name));
        }
        let field = snake(&c.name[2..]);
        let (maj, min) = c.core;
        match Self::first_handle(c) {
            None => format!("(s.entry().fp_v1_{min}().{field})({a})"),
            Some((p, ty)) => {
                let id = format!("{}_id", p.rs);
                let ctx = match ty {
                    "VkInstance" => format!("s.instance({id})?"),
                    "VkPhysicalDevice" => format!("s.instance_of_physical({id})?"),
                    "VkDevice" | "VkQueue" | "VkCommandBuffer" => format!("s.device({id})?"),
                    t => panic!("{}: no dispatch rule for first parameter {t}", c.name),
                };
                assert_eq!(maj, 1);
                format!("{{ let ctx = {ctx}; (ctx.fp_v1_{min}().{field})({a}) }}")
            }
        }
    }

    fn emit_hooks_trait(&self, o: &mut String) {
        writeln!(o, "/// Host-side commands that the session implements itself instead of forwarding blindly.\n#[cfg(feature = \"host\")]\npub trait Hooks {{").unwrap();
        for c in &self.cmds {
            if HOST_HOOKS.contains(&c.name.as_str()) {
                let ret = if c.ret_result { "vk::Result" } else { "()" };
                let sig = Self::guest_sig(c);
                writeln!(o, "unsafe fn {}(&mut self, {sig}) -> {ret};", snake(&c.name)).unwrap();
            }
        }
        writeln!(o, "}}").unwrap();
    }

    fn emit_vk(&self) -> String {
        let mut o = String::new();
        writeln!(o, "// SPDX-License-Identifier: GPL-3.0-or-later\n// @generated by winbridge-gfx-vkgen from vk.xml (VK_HEADER_VERSION {}); do not edit.\n", self.reg.header_version).unwrap();
        writeln!(o, "#![allow(unused_variables, unused_mut, unused_imports, unused_assignments, dead_code, deprecated, non_snake_case, unused_parens, clippy::all)]\nuse crate::{{CHAIN_END, MAX_ENUM}};\n#[cfg(feature = \"guest\")]\nuse crate::{{GuestError, Link}};\n#[cfg(feature = \"host\")]\nuse crate::{{Host, Resolver}};\nuse ash::vk;\nuse ash::vk::Handle as _;\nuse winbridge_gfx_proto::codec::{{Arena, CodecError, Dec, Enc}};\n").unwrap();
        writeln!(o, "/// Command names in wire-id order.\npub const COMMAND_NAMES: &[&str] = &[").unwrap();
        for n in &self.names {
            writeln!(o, "\"{n}\",").unwrap();
        }
        writeln!(o, "];\n").unwrap();
        for b in &self.bodies {
            self.emit_struct_codecs(&mut o, b);
        }
        for r in &self.in_roots {
            self.emit_chain_functions(&mut o, r, true);
        }
        for r in &self.out_roots {
            self.emit_chain_functions(&mut o, r, false);
        }
        self.emit_hooks_trait(&mut o);
        for c in &self.cmds {
            self.emit_guest(&mut o, c);
            self.emit_host(&mut o, c);
        }
        writeln!(o, "/// Executes one serialized call and returns the serialized reply.\n#[cfg(feature = \"host\")]\npub unsafe fn dispatch<H: Host>(s: &mut H, req: &[u8]) -> Result<Vec<u8>, CodecError> {{\nlet mut d = Dec::new(req);\nlet cmd = d.u16()?;\nlet mut a = Arena::new();\nlet mut r = Enc::new();\nmatch cmd {{").unwrap();
        for (i, c) in self.cmds.iter().enumerate() {
            writeln!(o, "{i} => exec_{}(s, &mut d, &mut a, &mut r)?,", snake(&c.name)).unwrap();
        }
        writeln!(o, "t => return Err(CodecError::BadTag(t as u32)),\n}}\nd.finish()?;\nOk(r.into_vec())\n}}").unwrap();
        o
    }

    fn emit_shims(&self) -> String {
        let mut o = String::new();
        writeln!(o, "// SPDX-License-Identifier: GPL-3.0-or-later\n// @generated by winbridge-gfx-vkgen from vk.xml (VK_HEADER_VERSION {}); do not edit.\n", self.reg.header_version).unwrap();
        writeln!(o, "#![allow(unused_variables, clippy::all)]\nuse crate::Level;\nuse ash::vk;\nuse winbridge_gfx_vk as gv;\n").unwrap();
        for c in &self.cmds {
            if ICD_CUSTOM.contains(&c.name.as_str()) {
                continue;
            }
            let full: Vec<String> = c.params.iter().map(|p| format!("{}: {}", p.rs, p.rs_ty)).collect();
            let call_args: Vec<&str> = c.params.iter().filter(|p| !matches!(p.kind, PKind::Skip)).map(|p| p.rs.as_str()).collect();
            let ret = if c.ret_result { "vk::Result" } else { "()" };
            let fail = if !c.ret_result {
                "()"
            } else if c.name == "vkCreateInstance" {
                "vk::Result::ERROR_INCOMPATIBLE_DRIVER"
            } else {
                "vk::Result::ERROR_DEVICE_LOST"
            };
            writeln!(o, "pub unsafe extern \"system\" fn shim_{n}({sig}) -> {ret} {{\ncrate::with_link(|l| gv::{n}(l, {args})).unwrap_or({fail})\n}}",
                n = snake(&c.name), sig = full.join(", "), args = call_args.join(", ")).unwrap();
        }
        writeln!(o, "/// Entry point address and loader level of every forwarded command.\npub fn lookup(name: &[u8]) -> Option<(*const (), Level)> {{\nmatch name {{").unwrap();
        for c in &self.cmds {
            let target = if ICD_CUSTOM.contains(&c.name.as_str()) { format!("crate::custom::{}", snake(&c.name)) } else { format!("shim_{}", snake(&c.name)) };
            writeln!(o, "b\"{}\" => Some(({target} as *const (), Level::{})),", c.name, Self::level(c)).unwrap();
        }
        writeln!(o, "_ => None,\n}}\n}}").unwrap();
        o
    }
}
