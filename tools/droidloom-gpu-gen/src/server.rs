//! Server output: in-place patchers that turn a record into live Vulkan
//! arguments, the handlers that call the driver, and the dispatch table.
//!
//! A record is never parsed or copied. Offsets become pointers and ids
//! become driver handles where they lie, and the driver reads the record.

use std::fmt::Write as _;

use crate::model::{length_expression, Api, Cmd, Level, MemberKind, ParamKind, Pointee};
use crate::protocol::pfn_signature;
use crate::rust::{array_length, ident, type_name};

fn emit_struct(api: &Api, name: &str, out: &mut String) {
    let members = api.registry.struct_members(name).expect("structure");
    writeln!(out, "pub unsafe fn patch_{name}(s: &mut Server, rec: *mut u8, p: *mut {name}) {{").ok();
    for member in members {
        let field = ident(&member.name);
        let elements: String = member.array.iter().map(|d| format!("({})", array_length(d))).collect::<Vec<_>>().join(" * ");
        match api.member_kind(name, member) {
            MemberKind::PNext => {
                writeln!(out, "    patch_chain(s, rec, (&raw mut (*p).pNext).cast());").ok();
            }
            MemberKind::Handle if member.array.is_empty() => {
                writeln!(out, "    (*p).{field} = s.h((*p).{field});").ok();
            }
            MemberKind::Handle => {
                writeln!(out, "    for i in 0..{elements} {{\n        let h = (&raw mut (*p).{field}).cast::<u64>().add(i);\n        *h = s.h(*h);\n    }}").ok();
            }
            MemberKind::Dispatchable if member.array.is_empty() => {
                writeln!(out, "    (*p).{field} = s.h((*p).{field} as u64) as usize;").ok();
            }
            MemberKind::Dispatchable => {
                writeln!(out, "    for i in 0..{elements} {{\n        let h = (&raw mut (*p).{field}).cast::<usize>().add(i);\n        *h = s.h(*h as u64) as usize;\n    }}").ok();
            }
            MemberKind::Fd => {
                writeln!(out, "    if (*p).{field} >= 0 {{ (*p).{field} = s.take_fd(); }}").ok();
            }
            MemberKind::Pointer(pointee, len) => {
                writeln!(out, "    (*p).{field} = rt::at(rec, (*p).{field} as i64);").ok();
                let count = len.map_or_else(|| "1".to_owned(), |l| length_expression(&l, &|id| format!("(*p).{}", ident(id))));
                patch_pointee(api, &pointee, &format!("(*p).{field}"), &count, out, "    ");
            }
            MemberKind::Scratch(..) => {
                writeln!(out, "    (*p).{field} = rt::at(rec, (*p).{field} as i64);").ok();
            }
            MemberKind::Nested(inner) if api.needs_server(&inner) => {
                if member.array.is_empty() {
                    writeln!(out, "    patch_{inner}(s, rec, &raw mut (*p).{field});").ok();
                } else {
                    writeln!(out, "    for i in 0..{elements} {{\n        patch_{inner}(s, rec, (&raw mut (*p).{field}).cast::<{inner}>().add(i));\n    }}").ok();
                }
            }
            _ => {}
        }
    }
    out.push_str("}\n\n");
}

/// Patch the data at `pointer` (already a live pointer, possibly null).
fn patch_pointee(api: &Api, pointee: &Pointee, pointer: &str, count: &str, out: &mut String, indent: &str) {
    match pointee {
        Pointee::Struct(inner) if api.needs_server(inner) => {
            writeln!(out, "{indent}if !{pointer}.is_null() {{\n{indent}    for i in 0..{count} {{\n{indent}        patch_{inner}(s, rec, {pointer}.cast_mut().add(i));\n{indent}    }}\n{indent}}}").ok();
        }
        Pointee::Handles => {
            writeln!(out, "{indent}if !{pointer}.is_null() {{\n{indent}    for i in 0..{count} {{\n{indent}        let h = {pointer}.cast_mut().cast::<u64>().add(i);\n{indent}        *h = s.h(*h);\n{indent}    }}\n{indent}}}").ok();
        }
        Pointee::Dispatchables => {
            writeln!(out, "{indent}if !{pointer}.is_null() {{\n{indent}    for i in 0..{count} {{\n{indent}        let h = {pointer}.cast_mut().cast::<u64>().add(i);\n{indent}        *h = s.h(*h);\n{indent}    }}\n{indent}}}").ok();
        }
        Pointee::StrArray => {
            writeln!(out, "{indent}if !{pointer}.is_null() {{\n{indent}    for i in 0..{count} {{\n{indent}        let slot = {pointer}.cast_mut().cast::<i64>().add(i);\n{indent}        *slot = rt::at::<u8>(rec, *slot) as i64;\n{indent}    }}\n{indent}}}").ok();
        }
        _ => {}
    }
}

fn emit_chains(api: &Api, out: &mut String) {
    out.push_str("/// Turn an input chain's offsets into pointers and patch every structure.\npub unsafe fn patch_chain(s: &mut Server, rec: *mut u8, link: *mut *mut rt::Base) {\n    *link = rt::at(rec, *link as i64);\n    let p = *link;\n    if p.is_null() {\n        return;\n    }\n    match (*p).sType {\n");
    for (name, value) in api.chained() {
        if api.needs_server(name) {
            writeln!(out, "        {value} => patch_{name}(s, rec, p.cast()),").ok();
        }
    }
    out.push_str("        _ => patch_chain(s, rec, (&raw mut (*p).pNext).cast()),\n    }\n}\n\n");
    out.push_str("/// Link an output skeleton's chain, and the arrays its structures carry,\n/// for the driver to fill.\npub unsafe fn link_chain(rec: *mut u8, mut link: *mut *mut rt::Base) {\n    loop {\n        *link = rt::at(rec, *link as i64);\n        if (*link).is_null() {\n            return;\n        }\n        match (**link).sType {\n");
    for (name, value, _, array, _) in api.output_arrays() {
        let array = ident(&array);
        writeln!(out, "            {value} => {{\n                let p = (*link).cast::<{name}>();\n                (*p).{array} = rt::at(rec, (*p).{array} as i64);\n            }}").ok();
    }
    out.push_str("            _ => {}\n        }\n        link = (&raw mut (**link).pNext).cast();\n    }\n}\n\n");
}

fn emit_command(api: &Api, cmd: &Cmd, out: &mut String) {
    let name = &cmd.name;
    writeln!(out, "pub unsafe fn h_{name}(s: &mut Server, rec: *mut u8) -> *mut u8 {{\n    let f = rec.add(8).cast::<frame::{name}>();\n    let _ = f;").ok();
    let mut args = Vec::new();
    let mut after = String::new();
    for p in &cmd.params {
        let field = ident(&p.member.name);
        let count = |l: &Option<String>| {
            l.as_ref().map_or_else(|| "1".to_owned(), |l| {
                length_expression(l, &|id| {
                    let q = cmd.params.iter().find(|q| q.member.name == id).expect("length parameter");
                    if matches!(q.kind, ParamKind::Out { .. }) { format!("(*{})", ident(id)) } else { ident(id) }
                })
            })
        };
        let ty = type_name(api, &p.member.ty);
        match &p.kind {
            ParamKind::Value => writeln!(out, "    let {field} = (*f).{field};").ok(),
            ParamKind::FdIn => {
                writeln!(out, "    let {field} = s.take_fd();").ok();
                writeln!(after, "    libc::close({field});").ok();
                None
            }
            ParamKind::Array(_) => writeln!(out, "    let {field} = (&raw const (*f).{field}).cast::<{ty}>();").ok(),
            ParamKind::Handle => writeln!(out, "    let {field} = s.h((*f).{field});").ok(),
            ParamKind::Dispatchable => writeln!(out, "    let {field} = s.h((*f).{field}) as usize;").ok(),
            ParamKind::StreamOwner => writeln!(out, "    let {field} = s.cb;").ok(),
            ParamKind::Allocator => writeln!(out, "    let {field} = core::ptr::null();").ok(),
            ParamKind::In(pointee, l) => {
                let pointee_ty = match pointee {
                    Pointee::Str | Pointee::StrArray => {
                        if matches!(pointee, Pointee::StrArray) { "*const c_char".to_owned() } else { "c_char".to_owned() }
                    }
                    Pointee::Bytes => "c_void".to_owned(),
                    _ => ty.clone(),
                };
                writeln!(out, "    let {field}: *const {pointee_ty} = rt::at(rec, (*f).{field});").ok();
                patch_pointee(api, pointee, &field, &count(l), out, "    ");
                None
            }
            ParamKind::Created { len: None, .. } => {
                writeln!(out, "    let mut {field}: {ty} = 0;").ok();
                writeln!(after, "    s.set((*f).{field}, {field} as u64);").ok();
                args.push(format!("&raw mut {field}"));
                continue;
            }
            ParamKind::Created { len: Some(l), .. } => {
                let n = count(&Some(l.clone()));
                writeln!(out, "    let ids: *mut u64 = rt::at(rec, (*f).{field});\n    let mut {field}: Vec<{ty}> = vec![0 as {ty}; {n}];").ok();
                writeln!(after, "    for (i, h) in {field}.iter().enumerate() {{\n        s.set(*ids.add(i), *h as u64);\n    }}").ok();
                args.push(format!("{field}.as_mut_ptr()"));
                continue;
            }
            ParamKind::FdOut => {
                writeln!(out, "    let {field}: *mut c_int = rt::at(rec, (*f).{field});").ok();
                writeln!(after, "    s.send_fd(*{field});").ok();
                None
            }
            ParamKind::Out { pointee, len, .. } => {
                writeln!(out, "    let {field}: *mut {ty} = rt::at(rec, (*f).{field});").ok();
                if let Pointee::Struct(_) = pointee {
                    let n = count(len);
                    writeln!(out, "    if !{field}.is_null() {{\n        for i in 0..{n} {{\n            link_chain(rec, (&raw mut (*{field}.add(i)).pNext).cast());\n        }}\n    }}").ok();
                }
                None
            }
        };
        args.push(field);
    }
    let call = format!("(s.fns.{name})({})", args.join(", "));
    if cmd.ret == "void" {
        writeln!(out, "    {call};").ok();
    } else if cmd.sync {
        writeln!(out, "    let result = {call};\n    *rt::at::<{}>(rec, (*f).reply) = result;", type_name(api, &cmd.ret)).ok();
    } else {
        writeln!(out, "    let result = {call};\n    if result != VK_SUCCESS {{\n        rt::failed(\"{name}\", result);\n    }}").ok();
    }
    out.push_str(&after);
    out.push_str("    rt::next(rec)\n}\n\n");
}

pub fn emit(api: &Api) -> String {
    let mut out = String::new();
    writeln!(out, "// @generated by droidloom-gpu-gen from vk.xml {}. Do not edit.", api.registry.header_version).ok();
    out.push_str("#![allow(non_snake_case, unused_variables, unused_mut, clippy::all, clippy::pedantic)]\n\nuse droidloom_gpu_protocol::generated::{frame, op};\nuse droidloom_gpu_protocol::generated::vk::*;\n\nuse crate::rt::{self, Server};\n\n");
    for name in &api.structs {
        if api.needs_server(name) {
            emit_struct(api, name, &mut out);
        }
    }
    emit_chains(api, &mut out);
    for cmd in &api.commands {
        if !cmd.server_manual {
            emit_command(api, cmd, &mut out);
        }
    }

    out.push_str("/// Device extensions whose commands are not all forwarded: never shown.\n");
    writeln!(out, "pub const UNFORWARDED_EXTENSIONS: [&core::ffi::CStr; {}] = [", api.unforwarded.len()).ok();
    for name in &api.unforwarded {
        writeln!(out, "    c\"{name}\",").ok();
    }
    out.push_str("];\n\n");

    // Driver entry points, loaded per level. Missing ones abort by name.
    out.push_str("/// Driver entry points. Until loaded, each one aborts naming itself.\npub struct Fns {\n");
    for cmd in &api.commands {
        writeln!(out, "    pub {}: PFN_{},", cmd.name, cmd.name).ok();
    }
    out.push_str("}\n\n");
    for cmd in &api.commands {
        let signature = pfn_signature(api, cmd);
        let params = signature.trim_start_matches("unsafe extern \"system\" fn(").rsplit_once(')').map(|(p, r)| (p.to_owned(), r.to_owned())).unwrap_or_default();
        let unused: Vec<String> = cmd.params.iter().map(|p| format!("_{}", p.member.name)).collect();
        let typed: Vec<String> = params.0.split(", ").zip(&unused).map(|(decl, name)| format!("{name}: {}", decl.split_once(": ").map(|x| x.1).unwrap_or(decl))).collect();
        writeln!(out, "unsafe extern \"system\" fn missing_{}({}){} {{\n    rt::missing(\"{}\")\n}}", cmd.name, typed.join(", "), params.1, cmd.name).ok();
    }
    out.push_str("\nimpl Fns {\n    pub const fn missing() -> Self {\n        Self {\n");
    for cmd in &api.commands {
        writeln!(out, "            {0}: missing_{0},", cmd.name).ok();
    }
    out.push_str("        }\n    }\n\n");
    for (function, levels, getter) in [
        ("load_global", &[Level::Global][..], "gipa(0, name)"),
        // Device commands too: the loader's stubs dispatch each call to its
        // own device's entry points, which a driver may make per device.
        ("load_instance", &[Level::Instance, Level::Physical, Level::Device][..], "gipa(handle, name)"),
    ] {
        writeln!(out, "    pub unsafe fn {function}(&mut self, gipa: PFN_vkGetInstanceProcAddr, handle: usize) {{\n        let _ = handle;").ok();
        for cmd in api.commands.iter().filter(|c| levels.contains(&c.level)) {
            writeln!(
                out,
                "        let name = c\"{0}\".as_ptr();\n        if let Some(f) = {getter} {{ self.{0} = core::mem::transmute::<unsafe extern \"system\" fn(), PFN_{0}>(f); }}",
                cmd.name
            )
            .ok();
        }
        out.push_str("    }\n\n");
    }
    out.push_str("}\n\n");

    out.push_str("/// Record handlers indexed by opcode.\npub static TABLE: [rt::Handler; rt::SLOTS] = {\n    let mut table = [rt::unknown as rt::Handler; rt::SLOTS];\n");
    for (opcode, handler) in [("PAD", "pad"), ("STOP", "stop"), ("RECORD", "record")] {
        writeln!(out, "    table[droidloom_gpu_protocol::layout::op::{opcode} as usize] = crate::manual::{handler};").ok();
    }
    for cmd in &api.commands {
        let path = if cmd.server_manual { format!("crate::manual::h_{}", cmd.name) } else { format!("h_{}", cmd.name) };
        writeln!(out, "    table[op::{} as usize] = {path};", cmd.name).ok();
    }
    out.push_str("    table\n};\n");
    out
}
