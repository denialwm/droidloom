//! The parts of vk.xml the generator needs: types, commands, constants and
//! structure-type values. Elements for other APIs (Vulkan SC) are skipped.

use std::collections::HashMap;

use roxmltree::{Document, Node};

/// How a member or parameter is reached from its declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pointer {
    /// `T x`
    Value,
    /// `const T* x`
    Const,
    /// `T* x`
    Mut,
    /// `const T* const* x`
    ConstConst,
    /// `T** x`
    MutMut,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub ty: String,
    pub pointer: Pointer,
    /// Fixed array dimensions, outermost first.
    pub array: Vec<String>,
    /// `len`, with `altlen` preferred when the length is written in LaTeX.
    pub len: Option<String>,
    pub values: Option<String>,
    pub bitfield: bool,
}

#[derive(Clone, Debug)]
pub enum TypeDef {
    Struct { members: Vec<Member> },
    Union { members: Vec<Member> },
    Handle { dispatchable: bool },
    Enum { wide: bool },
    Bitmask { wide: bool },
    /// A typedef to a C type, such as `VkBool32` or `VkDeviceSize`.
    Base { target: String, pointer: bool },
    FuncPointer,
    /// Window-system and other platform types: opaque to the generator.
    Opaque,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub ret: String,
    pub params: Vec<Member>,
}

pub struct Registry {
    pub types: HashMap<String, TypeDef>,
    pub type_aliases: HashMap<String, String>,
    pub commands: HashMap<String, Command>,
    /// Command name → every alias that resolves to it.
    pub command_aliases: HashMap<String, Vec<String>>,
    /// API constant name → (C type, C value).
    pub constants: HashMap<String, (String, String)>,
    /// Enum value name → numeric value, for `VkStructureType` and `VkResult`.
    pub values: HashMap<String, i64>,
    pub header_version: String,
    /// Commands Vulkan 1.0 to 1.3 require, in registry order.
    pub core: Vec<String>,
    /// Types Vulkan 1.0 to 1.3 require.
    pub core_types: std::collections::HashSet<String>,
    /// Structure → the structures it may extend (`structextends`).
    pub extends: HashMap<String, Vec<String>>,
    pub device_extensions: Vec<DeviceExtension>,
}

pub struct DeviceExtension {
    pub name: String,
    /// Commands its unconditional requirements add.
    pub commands: Vec<String>,
    /// Types any of its requirements add.
    pub types: Vec<String>,
    pub depends: Option<String>,
}

fn for_vulkan(node: Node) -> bool {
    node.attribute("api").is_none_or(|api| api.split(',').any(|a| a == "vulkan"))
}

fn child_text(node: Node, tag: &str) -> Option<String> {
    node.children()
        .find(|child| child.has_tag_name(tag))
        .and_then(|child| child.text())
        .map(str::to_owned)
}

fn parse_member(node: Node) -> Member {
    let mut before = String::new();
    let mut between = String::new();
    let mut after = String::new();
    let mut ty = String::new();
    let mut name = String::new();
    let mut stage = 0;
    for child in node.children() {
        if child.is_text() {
            let text = child.text().unwrap_or_default();
            match stage {
                0 => before.push_str(text),
                1 => between.push_str(text),
                _ => after.push_str(text),
            }
        } else if child.has_tag_name("type") {
            ty = child.text().unwrap_or_default().to_owned();
            stage = 1;
        } else if child.has_tag_name("name") {
            name = child.text().unwrap_or_default().to_owned();
            stage = 2;
        } else if child.has_tag_name("enum") && stage == 2 {
            after.push_str(child.text().unwrap_or_default());
        }
    }
    let stars: String = between.chars().filter(|c| !c.is_whitespace()).collect();
    let constant = before.contains("const");
    let pointer = match stars.as_str() {
        "" => Pointer::Value,
        "*" if constant => Pointer::Const,
        "*" => Pointer::Mut,
        "*const*" => Pointer::ConstConst,
        "**" => Pointer::MutMut,
        other => panic!("unsupported declarator {other:?} on {name}"),
    };
    let mut array = Vec::new();
    let mut rest = after.trim();
    while let Some(open) = rest.strip_prefix('[') {
        let close = open.find(']').expect("unterminated array");
        array.push(open[..close].trim().to_owned());
        rest = open[close + 1..].trim();
    }
    let latex = node.attribute("len").is_some_and(|len| len.starts_with("latexmath"));
    let len = if latex { node.attribute("altlen") } else { node.attribute("len") };
    Member {
        name,
        ty,
        pointer,
        array,
        len: len.map(str::to_owned),
        values: node.attribute("values").map(str::to_owned),
        bitfield: rest.starts_with(':'),
    }
}

fn members(node: Node) -> Vec<Member> {
    node.children()
        .filter(|child| child.has_tag_name("member") && for_vulkan(*child))
        .map(parse_member)
        .collect()
}

/// Value of an enum extended by a feature or extension block.
fn extension_value(node: Node, extension_number: Option<i64>) -> Option<i64> {
    if let Some(value) = node.attribute("value") {
        return parse_integer(value);
    }
    let offset: i64 = node.attribute("offset")?.parse().ok()?;
    let number = node
        .attribute("extnumber")
        .and_then(|n| n.parse().ok())
        .or(extension_number)?;
    let value = 1_000_000_000 + (number - 1) * 1000 + offset;
    Some(if node.attribute("dir") == Some("-") { -value } else { value })
}

fn parse_integer(value: &str) -> Option<i64> {
    value.strip_prefix("0x").map_or_else(|| value.parse().ok(), |hex| i64::from_str_radix(hex, 16).ok())
}

impl Registry {
    pub fn parse(text: &str) -> Self {
        let document = Document::parse(text).expect("vk.xml parses");
        let root = document.root_element();
        let mut registry = Self {
            types: HashMap::new(),
            type_aliases: HashMap::new(),
            commands: HashMap::new(),
            command_aliases: HashMap::new(),
            constants: HashMap::new(),
            values: HashMap::new(),
            header_version: String::new(),
            core: Vec::new(),
            core_types: std::collections::HashSet::new(),
            extends: HashMap::new(),
            device_extensions: Vec::new(),
        };
        let mut wide_enums = std::collections::HashSet::new();
        let mut value_aliases = Vec::new();

        for section in root.children().filter(Node::is_element) {
            match section.tag_name().name() {
                "types" => {
                    for node in section.children().filter(|n| n.has_tag_name("type") && for_vulkan(*n)) {
                        registry.add_type(node);
                    }
                }
                "enums" => {
                    let name = section.attribute("name").unwrap_or_default();
                    if section.attribute("bitwidth") == Some("64") {
                        wide_enums.insert(name.to_owned());
                    }
                    for value in section.children().filter(|n| n.has_tag_name("enum") && for_vulkan(*n)) {
                        let value_name = value.attribute("name").unwrap_or_default().to_owned();
                        if name == "API Constants" {
                            if let (Some(ty), Some(v)) = (value.attribute("type"), value.attribute("value")) {
                                registry.constants.insert(value_name, (ty.to_owned(), v.to_owned()));
                            }
                        } else if let Some(alias) = value.attribute("alias") {
                            value_aliases.push((value_name, alias.to_owned()));
                        } else if let Some(v) = value.attribute("value").and_then(parse_integer) {
                            registry.values.insert(value_name, v);
                        }
                    }
                }
                "commands" => {
                    for node in section.children().filter(|n| n.has_tag_name("command") && for_vulkan(*n)) {
                        registry.add_command(node);
                    }
                }
                "feature" | "extensions" => {
                    let blocks: Vec<Node> = if section.has_tag_name("extensions") {
                        section.children().filter(|n| n.has_tag_name("extension")).collect()
                    } else {
                        vec![section]
                    };
                    if section.has_tag_name("feature")
                        && for_vulkan(section)
                        && section.attribute("number").and_then(|n| n.parse::<f32>().ok()).is_some_and(|n| n <= 1.3)
                    {
                        for command in section.descendants().filter(|n| n.has_tag_name("command")) {
                            let name = command.attribute("name").unwrap_or_default().to_owned();
                            if !registry.core.contains(&name) {
                                registry.core.push(name);
                            }
                        }
                        for ty in section.descendants().filter(|n| n.has_tag_name("type")) {
                            registry.core_types.insert(ty.attribute("name").unwrap_or_default().to_owned());
                        }
                    }
                    for block in blocks {
                        let supported = block.attribute("supported").is_some_and(|s| s.split(',').any(|a| a == "vulkan"));
                        if block.has_tag_name("extension") && supported && block.attribute("type") == Some("device") {
                            let commands = block
                                .children()
                                .filter(|n| n.has_tag_name("require") && n.attribute("depends").is_none() && for_vulkan(*n))
                                .flat_map(|require| require.children().filter(|n| n.has_tag_name("command")))
                                .map(|command| command.attribute("name").unwrap_or_default().to_owned())
                                .collect();
                            let types = block
                                .children()
                                .filter(|n| n.has_tag_name("require") && for_vulkan(*n))
                                .flat_map(|require| require.children().filter(|n| n.has_tag_name("type")))
                                .map(|ty| ty.attribute("name").unwrap_or_default().to_owned())
                                .collect();
                            registry.device_extensions.push(DeviceExtension {
                                name: block.attribute("name").unwrap_or_default().to_owned(),
                                commands,
                                types,
                                depends: block.attribute("depends").map(str::to_owned),
                            });
                        }
                        let number = block.attribute("number").and_then(|n| n.parse().ok());
                        let enums = block
                            .descendants()
                            .filter(|n| n.has_tag_name("enum") && n.attribute("extends").is_some() && for_vulkan(*n));
                        for value in enums {
                            let value_name = value.attribute("name").unwrap_or_default().to_owned();
                            if let Some(alias) = value.attribute("alias") {
                                value_aliases.push((value_name, alias.to_owned()));
                            } else if let Some(v) = extension_value(value, number) {
                                registry.values.insert(value_name, v);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for _ in 0..4 {
            for (name, alias) in &value_aliases {
                if let Some(&value) = registry.values.get(alias) {
                    registry.values.entry(name.clone()).or_insert(value);
                }
            }
        }
        for (name, def) in &mut registry.types {
            if let TypeDef::Enum { wide } = def {
                *wide = wide_enums.contains(name);
            }
        }
        registry
    }

    fn add_type(&mut self, node: Node) {
        let category = node.attribute("category").unwrap_or_default();
        let name = node
            .attribute("name")
            .map(str::to_owned)
            .or_else(|| child_text(node, "name"))
            .unwrap_or_default();
        if let Some(alias) = node.attribute("alias") {
            self.type_aliases.insert(name, alias.to_owned());
            return;
        }
        if name == "VK_HEADER_VERSION" {
            let text: String = node.descendants().filter_map(|n| n.text()).collect();
            self.header_version = text.split_whitespace().last().unwrap_or_default().to_owned();
        }
        if let Some(parents) = node.attribute("structextends") {
            self.extends.insert(name.clone(), parents.split(',').map(str::to_owned).collect());
        }
        let def = match category {
            "struct" => TypeDef::Struct { members: members(node) },
            "union" => TypeDef::Union { members: members(node) },
            "handle" => TypeDef::Handle {
                dispatchable: child_text(node, "type").as_deref() == Some("VK_DEFINE_HANDLE"),
            },
            "enum" => TypeDef::Enum { wide: false },
            "bitmask" => TypeDef::Bitmask {
                wide: child_text(node, "type").as_deref() == Some("VkFlags64"),
            },
            "basetype" => match child_text(node, "type") {
                Some(target) => {
                    let text: String = node.children().filter_map(|n| n.text()).collect();
                    TypeDef::Base { pointer: text.contains('*'), target }
                }
                None => TypeDef::Opaque,
            },
            "funcpointer" => TypeDef::FuncPointer,
            "include" | "define" => return,
            _ => TypeDef::Opaque,
        };
        if !name.is_empty() {
            self.types.insert(name, def);
        }
    }

    fn add_command(&mut self, node: Node) {
        if let (Some(name), Some(alias)) = (node.attribute("name"), node.attribute("alias")) {
            self.command_aliases.entry(alias.to_owned()).or_default().push(name.to_owned());
            return;
        }
        let proto = node.children().find(|n| n.has_tag_name("proto")).expect("command proto");
        let command = Command {
            name: child_text(proto, "name").expect("command name"),
            ret: child_text(proto, "type").expect("command return type"),
            params: node
                .children()
                .filter(|n| n.has_tag_name("param") && for_vulkan(*n))
                .map(parse_member)
                .collect(),
        };
        self.commands.insert(command.name.clone(), command);
    }

    /// Follow type aliases to the defining name.
    pub fn resolve<'a>(&'a self, mut name: &'a str) -> &'a str {
        while let Some(target) = self.type_aliases.get(name) {
            name = target;
        }
        name
    }

    pub fn def(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(self.resolve(name))
    }

    pub fn is_dispatchable(&self, name: &str) -> bool {
        matches!(self.def(name), Some(TypeDef::Handle { dispatchable: true }))
    }

    pub fn is_handle(&self, name: &str) -> bool {
        matches!(self.def(name), Some(TypeDef::Handle { .. }))
    }

    pub fn struct_members(&self, name: &str) -> Option<&[Member]> {
        match self.def(name) {
            Some(TypeDef::Struct { members } | TypeDef::Union { members }) => Some(members),
            _ => None,
        }
    }

    pub fn is_union(&self, name: &str) -> bool {
        matches!(self.def(name), Some(TypeDef::Union { .. }))
    }
}
