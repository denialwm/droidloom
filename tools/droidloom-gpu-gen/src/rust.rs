//! Rust spellings of Vulkan declarations.

use crate::model::{c_type, Api};
use crate::registry::{Member, Pointer, TypeDef};

pub fn ident(name: &str) -> String {
    match name {
        "type" | "ref" | "match" | "fn" | "loop" | "move" | "use" | "mod" | "impl" | "box" => format!("r#{name}"),
        _ => name.to_owned(),
    }
}

/// The Rust name of a Vulkan or C type, with aliases resolved.
pub fn type_name(api: &Api, name: &str) -> String {
    let resolved = api.registry.resolve(name);
    c_type(resolved).map_or_else(|| resolved.to_owned(), str::to_owned)
}

/// The declared type of a member or parameter, pointers and arrays included.
pub fn declared(api: &Api, member: &Member) -> String {
    let base = type_name(api, &member.ty);
    let mut ty = match member.pointer {
        Pointer::Value => base,
        Pointer::Const => format!("*const {base}"),
        Pointer::Mut => format!("*mut {base}"),
        Pointer::ConstConst => format!("*const *const {base}"),
        Pointer::MutMut => format!("*mut *mut {base}"),
    };
    for dim in member.array.iter().rev() {
        ty = format!("[{ty}; {}]", array_length(dim));
    }
    ty
}

pub fn array_length(dim: &str) -> String {
    if dim.starts_with("VK_") { format!("{dim} as usize") } else { dim.to_owned() }
}

/// The scalar a plain type stands for, for `pub type` aliases.
pub fn scalar_definition(api: &Api, name: &str) -> String {
    if name == "PFN_vkVoidFunction" {
        return "Option<unsafe extern \"system\" fn()>".into();
    }
    match api.registry.def(name) {
        Some(TypeDef::Handle { dispatchable: true }) => "usize".into(),
        Some(TypeDef::Handle { dispatchable: false }) => "u64".into(),
        Some(TypeDef::Enum { wide: false }) => "i32".into(),
        Some(TypeDef::Enum { wide: true }) => "u64".into(),
        Some(TypeDef::Bitmask { wide: false }) => "VkFlags".into(),
        Some(TypeDef::Bitmask { wide: true }) => "VkFlags64".into(),
        Some(TypeDef::Base { target, pointer: true }) => format!("*mut {}", type_name(api, target)),
        Some(TypeDef::Base { target, pointer: false }) => type_name(api, target),
        Some(TypeDef::FuncPointer | TypeDef::Opaque) => "usize".into(),
        Some(TypeDef::Struct { .. } | TypeDef::Union { .. }) | None => panic!("{name} is not a scalar"),
    }
}

pub fn constant(c_ty: &str, value: &str) -> (String, String) {
    let ty = match c_ty {
        "uint32_t" => "u32",
        "uint64_t" => "u64",
        "int32_t" => "i32",
        "float" => "f32",
        other => panic!("constant of type {other}"),
    };
    let value = value.trim_matches(|c| c == '(' || c == ')');
    let rust = if let Some(inverted) = value.strip_prefix('~') {
        format!("!{}{ty}", inverted.trim_end_matches(['U', 'L']))
    } else if ty == "f32" {
        format!("{}{ty}", value.trim_end_matches('F'))
    } else {
        format!("{}{ty}", value.trim_end_matches(['U', 'L']))
    };
    (ty.to_owned(), rust)
}
