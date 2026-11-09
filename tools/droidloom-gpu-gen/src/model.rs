//! What each allowed command and structure means for the wire: which
//! parameters travel by value, which are deep-copied, which come back.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::registry::{Command, Member, Pointer, Registry, TypeDef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Global,
    Instance,
    Physical,
    Device,
}

/// Data behind an input pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pointee {
    Struct(String),
    /// Plain data copied as bytes: scalars, enums and structures without
    /// pointers, handles or chains.
    Pod(String),
    Str,
    StrArray,
    Bytes,
    Handles,
    Dispatchables,
}

#[derive(Clone, Debug)]
pub enum ParamKind {
    Value,
    /// `const float x[4]`: inline in the frame, passed by pointer.
    Array(String),
    Handle,
    Dispatchable,
    /// The command buffer that owns a stream command; implied by the stream.
    StreamOwner,
    Allocator,
    In(Pointee, Option<String>),
    Created { dispatchable: bool, len: Option<String> },
    /// A descriptor the caller keeps: lent to the host for the call.
    FdIn,
    FdOut,
    /// Written by the host into the caller's reply area. `inout` counts
    /// carry the caller's capacity in.
    Out { pointee: Pointee, len: Option<String>, inout: bool },
}

#[derive(Clone, Debug)]
pub struct Param {
    pub member: Member,
    pub kind: ParamKind,
}

#[derive(Clone, Debug)]
pub struct Cmd {
    pub name: String,
    pub ret: String,
    pub params: Vec<Param>,
    pub aliases: Vec<String>,
    pub level: Level,
    pub stream: bool,
    pub sync: bool,
    pub client_manual: bool,
    pub server_manual: bool,
    pub opcode: u32,
    /// Index of the parameter whose handle this command destroys.
    pub destroys: Option<usize>,
}

/// Classification of one structure member.
#[derive(Clone, Debug)]
pub enum MemberKind {
    SType,
    PNext,
    Pod,
    Handle,
    Dispatchable,
    Fd,
    Nested(String),
    Pointer(Pointee, Option<String>),
    /// Room the host writes into from an input structure (creation
    /// feedback): reserved in the record, never copied back, so the
    /// application keeps what it put there.
    Scratch(String, Option<String>),
    /// Function pointers and other members the wire never carries.
    Unsupported,
}

pub struct Api<'r> {
    pub registry: &'r Registry,
    pub commands: Vec<Cmd>,
    /// Commands the client answers alone: no record, no host side.
    pub local: Vec<(String, Level)>,
    /// Core commands not forwarded yet: the client aborts naming them.
    pub missing: Vec<(String, Level)>,
    /// Device extensions with commands that are not forwarded: hidden from
    /// the application.
    pub unforwarded: Vec<String>,
    /// Every structure and union reachable from the allowed commands, in a
    /// stable order.
    pub structs: Vec<String>,
    /// Plain type names (handles, enums, bitmasks, base types) reached.
    pub scalars: BTreeSet<String>,
    pub constants: BTreeSet<String>,
}

/// First opcode free for generated commands; lower values are the
/// transport's own records.
pub const FIRST_OPCODE: u32 = 16;

pub fn c_type(name: &str) -> Option<&'static str> {
    Some(match name {
        "void" => "c_void",
        "char" => "c_char",
        "float" => "f32",
        "double" => "f64",
        "int8_t" => "i8",
        "uint8_t" => "u8",
        "int16_t" => "i16",
        "uint16_t" => "u16",
        "int32_t" => "i32",
        "uint32_t" => "u32",
        "int64_t" => "i64",
        "uint64_t" => "u64",
        "int" => "c_int",
        "size_t" => "usize",
        _ => return None,
    })
}

/// Dispatch level from the first parameter.
fn level_of(registry: &Registry, command: &Command) -> Level {
    match command.params.first().map(|p| registry.resolve(&p.ty)) {
        Some("VkInstance") => Level::Instance,
        Some("VkPhysicalDevice") => Level::Physical,
        Some("VkDevice" | "VkQueue" | "VkCommandBuffer") => Level::Device,
        _ => Level::Global,
    }
}

impl<'r> Api<'r> {
    pub fn build(registry: &'r Registry, allowlist: &str) -> Self {
        let mut api = Api {
            registry,
            commands: Vec::new(),
            local: Vec::new(),
            missing: Vec::new(),
            unforwarded: Vec::new(),
            structs: Vec::new(),
            scalars: BTreeSet::new(),
            constants: BTreeSet::new(),
        };
        let mut seen = HashSet::new();
        let mut opcode = FIRST_OPCODE;
        for line in allowlist.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("command"), Some(name)) => {
                    let flags: Vec<&str> = words.collect();
                    let command = registry
                        .commands
                        .get(name)
                        .unwrap_or_else(|| panic!("{name} is not a Vulkan command (aliases must use the core name)"));
                    if flags.contains(&"local") {
                        api.local.push((command.name.clone(), level_of(registry, command)));
                        for ty in command.params.iter().map(|p| p.ty.clone()).chain([command.ret.clone()]) {
                            api.reach(&ty, &mut seen);
                        }
                        continue;
                    }
                    let cmd = api.classify(command, &flags, opcode);
                    opcode += 1;
                    api.commands.push(cmd);
                }
                (Some("struct"), Some(name)) => api.reach(name, &mut seen),
                (None, None) => {}
                _ => panic!("bad allowlist line: {line}"),
            }
        }
        let reached: Vec<String> = api
            .commands
            .iter()
            .flat_map(|cmd| cmd.params.iter().map(|p| p.member.ty.clone()).chain([cmd.ret.clone()]))
            .chain(["VkResult".to_owned(), "VkStructureType".to_owned()])
            .collect();
        for ty in reached {
            api.reach(&ty, &mut seen);
        }
        let covered: HashSet<&str> =
            api.commands.iter().map(|c| c.name.as_str()).chain(api.local.iter().map(|(name, _)| name.as_str())).collect();
        let missing: Vec<(String, Level)> = registry
            .core
            .iter()
            .filter(|name| !covered.contains(name.as_str()))
            .filter_map(|name| registry.commands.get(name))
            .map(|command| (command.name.clone(), level_of(registry, command)))
            .collect();
        api.missing = missing;
        let core_name: HashMap<&str, &str> = registry
            .command_aliases
            .iter()
            .flat_map(|(core, aliases)| aliases.iter().map(move |alias| (alias.as_str(), core.as_str())))
            .collect();
        let mut hidden: HashSet<&str> = registry
            .device_extensions
            .iter()
            .filter(|e| e.commands.iter().any(|name| !covered.contains(core_name.get(name.as_str()).copied().unwrap_or(name))))
            .map(|e| e.name.as_str())
            .collect();
        // An extension needing a hidden one is hidden too, until nothing changes.
        loop {
            let before = hidden.len();
            for extension in &registry.device_extensions {
                if extension.depends.as_deref().is_some_and(|expression| !satisfied(expression, &hidden)) {
                    hidden.insert(&extension.name);
                }
            }
            if hidden.len() == before {
                break;
            }
        }
        // Every structure that may extend a forwarded one travels too, from the
        // core and from each advertised extension: an application that sees an
        // extension fills its structures in, and dropping them would change
        // what the host does.
        let advertised: HashSet<&str> = registry
            .device_extensions
            .iter()
            .filter(|e| !hidden.contains(e.name.as_str()))
            .flat_map(|e| e.types.iter().map(String::as_str))
            .collect();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for (child, parents) in &registry.extends {
            if registry.core_types.contains(child) || advertised.contains(child.as_str()) {
                for parent in parents {
                    children.entry(parent.as_str()).or_default().push(child.as_str());
                }
            }
        }
        for list in children.values_mut() {
            list.sort();
        }
        let mut next = 0;
        while next < api.structs.len() {
            let parent = api.structs[next].clone();
            for child in children.get(parent.as_str()).into_iter().flatten() {
                api.reach(child, &mut seen);
            }
            next += 1;
        }
        api.unforwarded =
            registry.device_extensions.iter().filter(|e| hidden.contains(e.name.as_str())).map(|e| e.name.clone()).collect();
        api
    }

    fn reach(&mut self, name: &str, seen: &mut HashSet<String>) {
        let resolved = self.registry.resolve(name).to_owned();
        if c_type(&resolved).is_some() || !seen.insert(resolved.clone()) {
            return;
        }
        match self.registry.def(&resolved) {
            Some(TypeDef::Struct { members } | TypeDef::Union { members }) => {
                for member in members.clone() {
                    for dim in &member.array {
                        if dim.starts_with("VK_") {
                            self.constants.insert(dim.clone());
                        }
                    }
                    self.reach(&member.ty, seen);
                }
                self.structs.push(resolved);
            }
            Some(TypeDef::Base { target, .. }) => {
                let target = target.clone();
                self.reach(&target, seen);
                self.scalars.insert(resolved);
            }
            Some(_) => {
                self.scalars.insert(resolved);
            }
            None => panic!("unknown type {name}"),
        }
    }

    fn classify(&self, command: &Command, flags: &[&str], opcode: u32) -> Cmd {
        let registry = self.registry;
        let first = command.params.first().map(|p| registry.resolve(&p.ty).to_owned());
        let level = level_of(registry, command);
        let stream = first.as_deref() == Some("VkCommandBuffer") && command.name != "vkResetCommandBuffer";
        let counts: HashSet<&str> = command
            .params
            .iter()
            .filter_map(|p| p.len.as_deref())
            .filter(|len| command.params.iter().any(|q| q.name == *len && q.pointer == Pointer::Mut))
            .collect();
        let params: Vec<Param> = command
            .params
            .iter()
            .enumerate()
            .map(|(index, member)| Param {
                kind: self.param_kind(command, index, member, stream, counts.contains(member.name.as_str())),
                member: member.clone(),
            })
            .collect();
        let has_outs = params
            .iter()
            .any(|p| matches!(p.kind, ParamKind::Out { .. } | ParamKind::FdOut));
        let returns_value = !matches!(command.ret.as_str(), "void" | "VkResult");
        let destroys = (command.name.starts_with("vkDestroy") || command.name.starts_with("vkFree"))
            .then(|| {
                // The last handle: vkFreeDescriptorSets names the pool first.
                params.iter().rposition(|p| {
                    matches!(p.kind, ParamKind::Handle | ParamKind::In(Pointee::Handles, _))
                        && registry.resolve(&p.member.ty) != "VkDevice"
                })
            })
            .flatten()
            .or_else(|| {
                matches!(command.name.as_str(), "vkDestroyDevice" | "vkDestroyInstance").then_some(0)
            });
        Cmd {
            name: command.name.clone(),
            ret: command.ret.clone(),
            aliases: registry.command_aliases.get(&command.name).cloned().unwrap_or_default(),
            level,
            stream,
            sync: has_outs || returns_value || flags.contains(&"sync"),
            client_manual: flags.contains(&"client"),
            server_manual: flags.contains(&"server"),
            opcode,
            destroys,
            params,
        }
    }

    fn param_kind(&self, command: &Command, index: usize, member: &Member, stream: bool, inout: bool) -> ParamKind {
        let registry = self.registry;
        let ty = registry.resolve(&member.ty);
        if index == 0 && stream {
            return ParamKind::StreamOwner;
        }
        if ty == "VkAllocationCallbacks" {
            return ParamKind::Allocator;
        }
        if !member.array.is_empty() {
            assert!(member.array.len() == 1, "{}: multi-dimensional array parameter", command.name);
            return ParamKind::Array(member.array[0].clone());
        }
        match member.pointer {
            Pointer::Value if registry.is_dispatchable(ty) => ParamKind::Dispatchable,
            Pointer::Value if registry.is_handle(ty) => ParamKind::Handle,
            Pointer::Value if member.name == "fd" && ty == "int" => ParamKind::FdIn,
            Pointer::Value => ParamKind::Value,
            Pointer::Const | Pointer::ConstConst => {
                ParamKind::In(self.pointee(member, &command.name), self.length(member))
            }
            Pointer::Mut if matches!(member.name.as_str(), "pFd" | "pNativeFenceFd") && ty == "int" => ParamKind::FdOut,
            Pointer::Mut if registry.is_handle(ty) && !command.name.starts_with("vkEnumerate") && !command.name.starts_with("vkGet")
                || command.name == "vkGetDeviceQueue" =>
            {
                ParamKind::Created { dispatchable: registry.is_dispatchable(ty), len: self.length(member) }
            }
            Pointer::Mut => ParamKind::Out { pointee: self.pointee(member, &command.name), len: self.length(member), inout },
            Pointer::MutMut => panic!("{}: {} is a pointer to pointer output", command.name, member.name),
        }
    }

    fn length(&self, member: &Member) -> Option<String> {
        member
            .len
            .as_deref()
            .map(|len| len.trim_end_matches(",null-terminated").to_owned())
            .filter(|len| len != "null-terminated" && len != "1")
    }

    fn pointee(&self, member: &Member, owner: &str) -> Pointee {
        let registry = self.registry;
        let ty = registry.resolve(&member.ty);
        if member.pointer == Pointer::ConstConst {
            assert!(ty == "char", "{owner}: {} is an array of pointers to {ty}", member.name);
            return Pointee::StrArray;
        }
        if ty == "char" {
            return Pointee::Str;
        }
        if ty == "void" {
            return Pointee::Bytes;
        }
        if registry.is_dispatchable(ty) {
            return Pointee::Dispatchables;
        }
        if registry.is_handle(ty) {
            return Pointee::Handles;
        }
        if registry.struct_members(ty).is_some() && (self.needs_client(ty) || self.needs_server(ty)) {
            return Pointee::Struct(ty.to_owned());
        }
        Pointee::Pod(ty.to_owned())
    }

    pub fn member_kind(&self, owner: &str, member: &Member) -> MemberKind {
        let registry = self.registry;
        let ty = registry.resolve(&member.ty);
        if member.bitfield {
            return MemberKind::Pod;
        }
        if member.name == "sType" && member.values.is_some() {
            return MemberKind::SType;
        }
        if member.name == "pNext" {
            return MemberKind::PNext;
        }
        if matches!(registry.def(ty), Some(TypeDef::FuncPointer)) {
            return MemberKind::Unsupported;
        }
        match member.pointer {
            Pointer::Value if member.name == "fd" && ty == "int" => MemberKind::Fd,
            Pointer::Value if registry.is_dispatchable(ty) => MemberKind::Dispatchable,
            Pointer::Value if registry.is_handle(ty) => MemberKind::Handle,
            Pointer::Value if registry.struct_members(ty).is_some() && !registry.is_union(ty) => {
                if self.needs_client(ty) || self.needs_server(ty) {
                    MemberKind::Nested(ty.to_owned())
                } else {
                    MemberKind::Pod
                }
            }
            Pointer::Value => MemberKind::Pod,
            Pointer::Const | Pointer::ConstConst => MemberKind::Pointer(self.pointee(member, owner), self.length(member)),
            Pointer::Mut if ty != "void" => MemberKind::Scratch(ty.to_owned(), self.length(member)),
            // Opaque pointers inside input structures (pUserData and similar).
            Pointer::Mut | Pointer::MutMut => MemberKind::Unsupported,
        }
    }

    /// Whether the client must do more than copy the bytes.
    pub fn needs_client(&self, ty: &str) -> bool {
        let Some(members) = self.registry.struct_members(ty) else { return false };
        if self.registry.is_union(ty) {
            return false;
        }
        members.iter().any(|m| match self.member_kind(ty, m) {
            MemberKind::PNext | MemberKind::Dispatchable | MemberKind::Fd | MemberKind::Pointer(..) | MemberKind::Scratch(..) => true,
            MemberKind::Nested(inner) => self.needs_client(&inner),
            _ => false,
        })
    }

    /// Whether the server must patch the structure in place before use.
    pub fn needs_server(&self, ty: &str) -> bool {
        let Some(members) = self.registry.struct_members(ty) else { return false };
        if self.registry.is_union(ty) {
            return false;
        }
        members.iter().any(|m| match self.member_kind(ty, m) {
            MemberKind::PNext
            | MemberKind::Handle
            | MemberKind::Dispatchable
            | MemberKind::Fd
            | MemberKind::Pointer(..)
            | MemberKind::Scratch(..) => true,
            MemberKind::Nested(inner) => self.needs_server(&inner),
            _ => false,
        })
    }

    /// Structures that may appear in a pNext chain: they carry a fixed sType.
    pub fn chained(&self) -> Vec<(&str, String)> {
        self.structs
            .iter()
            .filter_map(|name| {
                let members = self.registry.struct_members(name)?;
                let first = members.first()?;
                if first.name != "sType" {
                    return None;
                }
                Some((name.as_str(), first.values.clone()?))
            })
            .collect()
    }

    /// Output structures of a chain that carry an array the caller provides
    /// and the host fills: (structure, sType, count member, array member,
    /// element type). The count goes in as the array's capacity and comes
    /// back as what was written; a null array asks for the count alone.
    pub fn output_arrays(&self) -> Vec<(&str, String, String, String, String)> {
        let mut arrays = Vec::new();
        for (name, value) in self.chained() {
            if !self.registry.returned.contains(name) {
                continue;
            }
            for member in self.registry.struct_members(name).into_iter().flatten() {
                if let MemberKind::Scratch(element, Some(count)) = self.member_kind(name, member) {
                    arrays.push((name, value.clone(), count, member.name.clone(), element));
                }
            }
        }
        arrays
    }
}

/// Translate a vk.xml length expression to a Rust `usize` expression.
/// `field` maps an identifier to the place that holds it.
pub fn length_expression(expression: &str, field: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut chars = expression.chars().peekable();
    let mut base = None::<String>;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() || c == '_' {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if !(c.is_ascii_alphanumeric() || c == '_') {
                    break;
                }
                word.push(c);
                chars.next();
            }
            if let Some(place) = base.take() {
                out.push_str(&format!("((*{place}).{word} as usize)"));
            } else if chars.clone().take(2).eq("->".chars()) {
                chars.next();
                chars.next();
                base = Some(field(&word));
            } else if word.starts_with("VK_") {
                out.push_str(&format!("({word} as usize)"));
            } else {
                out.push_str(&format!("({} as usize)", field(&word)));
            }
        } else {
            out.push(c);
            chars.next();
        }
    }
    // A lone term needs no parentheses: rustc warns about them.
    match out.strip_prefix('(').and_then(|inner| inner.strip_suffix(" as usize)")) {
        Some(inner) if !inner.contains(" as usize") => format!("{inner} as usize"),
        _ => out,
    }
}

/// Evaluate a registry `depends` expression (`+` and, `,` or, parentheses):
/// versions and instance extensions hold; device extensions unless hidden.
fn satisfied(expression: &str, hidden: &HashSet<&str>) -> bool {
    fn any(tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>, hidden: &HashSet<&str>) -> bool {
        let mut value = all(tokens, hidden);
        while tokens.peek().is_some_and(|t| t == ",") {
            tokens.next();
            value |= all(tokens, hidden);
        }
        value
    }
    fn all(tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>, hidden: &HashSet<&str>) -> bool {
        let mut value = term(tokens, hidden);
        while tokens.peek().is_some_and(|t| t == "+") {
            tokens.next();
            value &= term(tokens, hidden);
        }
        value
    }
    fn term(tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>, hidden: &HashSet<&str>) -> bool {
        match tokens.next() {
            Some(open) if open == "(" => {
                let value = any(tokens, hidden);
                tokens.next();
                value
            }
            Some(name) => !hidden.contains(name.as_str()),
            None => true,
        }
    }
    let mut tokens = Vec::new();
    let mut word = String::new();
    for c in expression.chars() {
        if matches!(c, '+' | ',' | '(' | ')') {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            tokens.push(c.to_string());
        } else if !c.is_whitespace() {
            word.push(c);
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    any(&mut tokens.into_iter().peekable(), hidden)
}
