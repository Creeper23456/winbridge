// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal model of the parts of `vk.xml` the generator needs.

use roxmltree::{Document, Node};
use std::collections::{BTreeMap, HashMap};

/// A struct member or command parameter.
#[derive(Debug, Clone)]
pub struct Decl {
    pub name: String,
    pub ty: String,
    /// `const` appears before the type.
    pub base_const: bool,
    /// Number of `*` after the type.
    pub ptr: usize,
    pub is_array: bool,
    /// Element count of a fixed-size array parameter (`float blendConstants[4]`).
    pub fixed_len: Option<usize>,
    pub bitfield: bool,
    /// First element of `len=`, if any (a sibling name, a number, or `latexmath`).
    pub len: Option<String>,
    pub null_terminated: bool,
    /// `values=` of an `sType` member.
    pub values: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TyKind {
    /// C builtin, enum, bitmask or base type: plain scalar bytes.
    Scalar,
    Handle { dispatchable: bool },
    Struct,
    Union,
    FuncPtr,
}

#[derive(Debug, Clone)]
pub struct Struct {
    pub name: String,
    pub members: Vec<Decl>,
    pub extends: Vec<String>,
}

impl Struct {
    /// `sType` first and `pNext` second.
    pub fn has_header(&self) -> bool {
        self.members.len() >= 2 && self.members[0].name == "sType" && self.members[1].name == "pNext"
    }
}

#[derive(Debug, Clone)]
pub struct Command {
    pub ret: String,
    pub params: Vec<Decl>,
    /// Core version that introduced the command (`1.0`, `1.1`, ...), if any.
    pub core: Option<(u32, u32)>,
}

pub struct Registry {
    pub structs: BTreeMap<String, Struct>,
    pub commands: BTreeMap<String, Command>,
    tykinds: HashMap<String, TyKind>,
    pub header_version: String,
    /// Canonical `VK_STRUCTURE_TYPE_*` name to its alias names (`..._KHR`, `..._EXT`).
    stype_aliases: HashMap<String, Vec<String>>,
}

fn applies_to_vulkan(n: Node) -> bool {
    match n.attribute("api") {
        None => true,
        Some(a) => a.split(',').any(|x| x == "vulkan"),
    }
}

fn text_of(n: Node) -> String {
    n.descendants().filter(|d| d.is_text()).map(|d| d.text().unwrap()).collect()
}

fn parse_decl(n: Node) -> Decl {
    let (mut before, mut after_ty, mut after_name) = (String::new(), String::new(), String::new());
    let (mut ty, mut name) = (None, None);
    for c in n.children() {
        if c.is_text() {
            let t = c.text().unwrap();
            if name.is_some() {
                after_name.push_str(t);
            } else if ty.is_some() {
                after_ty.push_str(t);
            } else {
                before.push_str(t);
            }
        } else if c.is_element() {
            match c.tag_name().name() {
                "type" => ty = Some(text_of(c)),
                "name" => name = Some(text_of(c)),
                "enum" => after_name.push_str(&format!("[{}]", text_of(c))),
                _ => {}
            }
        }
    }
    let len_attr = n.attribute("len");
    let mut len = None;
    let mut null_terminated = false;
    if let Some(l) = len_attr {
        for (i, part) in l.split(',').enumerate() {
            if part == "null-terminated" {
                null_terminated = true;
            } else if i == 0 {
                len = Some(part.to_string());
            }
        }
    }
    Decl {
        name: name.expect("declaration without name"),
        ty: ty.expect("declaration without type"),
        base_const: before.contains("const"),
        ptr: after_ty.matches('*').count(),
        is_array: after_name.contains('['),
        fixed_len: after_name.split_once('[').and_then(|(_, t)| t.split_once(']')).and_then(|(n, _)| n.trim().parse().ok()),
        bitfield: after_name.contains(':'),
        len,
        null_terminated,
        values: n.attribute("values").map(str::to_string),
    }
}

impl Registry {
    pub fn load(xml: &str) -> Registry {
        let doc = Document::parse(xml).expect("vk.xml parses");
        let root = doc.root_element();
        let mut structs = BTreeMap::new();
        let mut tykinds: HashMap<String, TyKind> = HashMap::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        let mut header_version = String::new();

        for types in root.children().filter(|c| c.has_tag_name("types")) {
            for t in types.children().filter(|c| c.has_tag_name("type")) {
                if !applies_to_vulkan(t) {
                    continue;
                }
                let cat = t.attribute("category").unwrap_or("");
                if cat == "define" && text_of(t).contains("VK_HEADER_VERSION ") {
                    header_version = text_of(t).split_whitespace().last().unwrap_or("").to_string();
                }
                let name = t
                    .attribute("name")
                    .map(str::to_string)
                    .or_else(|| t.children().find(|c| c.has_tag_name("name")).map(text_of));
                let Some(name) = name else { continue };
                if let Some(alias) = t.attribute("alias") {
                    aliases.push((name, alias.to_string()));
                    continue;
                }
                match cat {
                    "struct" | "union" => {
                        let members = t
                            .children()
                            .filter(|c| c.has_tag_name("member") && applies_to_vulkan(*c))
                            .map(parse_decl)
                            .collect();
                        let extends = t
                            .attribute("structextends")
                            .map(|s| s.split(',').map(str::to_string).collect())
                            .unwrap_or_default();
                        tykinds.insert(name.clone(), if cat == "union" { TyKind::Union } else { TyKind::Struct });
                        structs.insert(name.clone(), Struct { name, members, extends });
                    }
                    "handle" => {
                        let disp = t.children().any(|c| c.has_tag_name("type") && text_of(c) == "VK_DEFINE_HANDLE");
                        tykinds.insert(name, TyKind::Handle { dispatchable: disp });
                    }
                    "funcpointer" => {
                        tykinds.insert(name, TyKind::FuncPtr);
                    }
                    _ => {
                        tykinds.insert(name, TyKind::Scalar);
                    }
                }
            }
        }
        for (name, target) in aliases {
            if let Some(k) = tykinds.get(&target).cloned() {
                tykinds.insert(name.clone(), k);
            }
            if let Some(s) = structs.get(&target).cloned() {
                structs.insert(name.clone(), Struct { name, ..s });
            }
        }

        let mut core_of: HashMap<String, (u32, u32)> = HashMap::new();
        for f in root.children().filter(|c| c.has_tag_name("feature") && applies_to_vulkan(*c)) {
            let Some(num) = f.attribute("number") else { continue };
            let Some((maj, min)) = num.split_once('.') else { continue };
            let v = (maj.parse().unwrap(), min.parse().unwrap());
            for c in f.descendants().filter(|d| d.has_tag_name("command")) {
                if let Some(n) = c.attribute("name") {
                    core_of.entry(n.to_string()).or_insert(v);
                }
            }
        }

        let mut commands = BTreeMap::new();
        for cs in root.children().filter(|c| c.has_tag_name("commands")) {
            for c in cs.children().filter(|c| c.has_tag_name("command") && c.attribute("alias").is_none()) {
                if !applies_to_vulkan(c) {
                    continue;
                }
                let proto = c.children().find(|x| x.has_tag_name("proto")).unwrap();
                let name = text_of(proto.children().find(|x| x.has_tag_name("name")).unwrap());
                let ret = text_of(proto.children().find(|x| x.has_tag_name("type")).unwrap());
                let params = c
                    .children()
                    .filter(|p| p.has_tag_name("param") && applies_to_vulkan(*p))
                    .map(parse_decl)
                    .collect();
                let core = core_of.get(&name).copied();
                commands.insert(name, Command { ret, params, core });
            }
        }
        // Vulkan 1.4 promoted types: the pinned ash only knows their `...KHR` names.
        for c in commands.values_mut() {
            for p in c.params.iter_mut() {
                if let Some((_, khr)) = crate::subset::PROMOTED_TYPES.iter().find(|(core, _)| *core == p.ty) {
                    p.ty = khr.to_string();
                }
            }
        }
        let mut stype_aliases: HashMap<String, Vec<String>> = HashMap::new();
        for e in root.descendants().filter(|d| d.has_tag_name("enum") && d.attribute("extends") == Some("VkStructureType")) {
            if let (Some(name), Some(target)) = (e.attribute("name"), e.attribute("alias")) {
                stype_aliases.entry(target.to_string()).or_default().push(name.to_string());
            }
        }
        Registry { structs, commands, tykinds, header_version, stype_aliases }
    }

    pub fn kind(&self, ty: &str) -> TyKind {
        self.tykinds.get(ty).cloned().unwrap_or(TyKind::Scalar)
    }

    /// ash's name for the structure type of `s`, e.g. `vk::StructureType::X_KHR` for an
    /// `...KHR` alias whose registry value is the promoted name.
    pub fn stype(&self, s: &Struct) -> String {
        let canon = s.members[0].values.as_ref().expect("sType without values");
        let suffix = ["KHR", "EXT"].into_iter().find(|x| s.name.ends_with(x));
        let chosen = suffix
            .and_then(|x| self.stype_aliases.get(canon).and_then(|v| v.iter().find(|n| n.ends_with(&format!("_{x}")))))
            .unwrap_or(canon);
        format!("vk::StructureType::{}", chosen.strip_prefix("VK_STRUCTURE_TYPE_").expect("sType prefix"))
    }

    /// Plain data: copying the bytes is a correct serialization.
    pub fn is_pod(&self, name: &str) -> bool {
        let s = &self.structs[name];
        let hdr = s.has_header();
        s.members.iter().enumerate().all(|(i, m)| {
            if hdr && i < 2 {
                return true;
            }
            if m.ptr > 0 {
                return false;
            }
            match self.kind(&m.ty) {
                TyKind::Scalar => true,
                TyKind::Struct | TyKind::Union => self.is_pod(&m.ty),
                TyKind::Handle { .. } | TyKind::FuncPtr => false,
            }
        })
    }
}
