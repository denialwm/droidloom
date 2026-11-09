//! Client output: deep-copy encoders and the Vulkan entry points that write
//! records. Every encoder runs twice, first measuring and then writing, so
//! a record is reserved once at its exact size.

use std::fmt::Write as _;

use crate::model::{length_expression, Api, Cmd, Level, MemberKind, ParamKind, Pointee};
use crate::protocol::{frame_type, pfn_signature};
use crate::rust::{array_length, ident, type_name};

fn pointee_type(api: &Api, pointee: &Pointee) -> String {
    match pointee {
        Pointee::Struct(name) | Pointee::Pod(name) => type_name(api, name),
        Pointee::Str | Pointee::Bytes => "u8".into(),
        Pointee::StrArray | Pointee::Handles | Pointee::Dispatchables => "u64".into(),
    }
}

/// Expression encoding the data behind `pointer`, returning its offset.
fn encode_pointee(api: &Api, pointee: &Pointee, pointer: &str, len: Option<&str>) -> String {
    let count = len.unwrap_or("1");
    match pointee {
        // Only the host patches it (handles): a plain copy here.
        Pointee::Struct(name) if !api.needs_client(name) => format!("rt::enc_pod::<{}, W>(w, {pointer}, {count})", type_name(api, name)),
        Pointee::Struct(name) if len.is_none() => format!("enc_{name}(w, {pointer})"),
        Pointee::Struct(name) => format!("enc_n_{name}(w, {pointer}, {count})"),
        Pointee::Pod(name) => format!("rt::enc_pod::<{}, W>(w, {pointer}, {count})", type_name(api, name)),
        Pointee::Str => format!("rt::enc_str(w, {pointer})"),
        Pointee::StrArray => format!("rt::enc_strs(w, {pointer}, {count})"),
        Pointee::Bytes => format!("rt::enc_bytes(w, {pointer}.cast(), {count})"),
        Pointee::Handles => format!("rt::enc_pod::<u64, W>(w, {pointer}, {count})"),
        Pointee::Dispatchables => format!("rt::enc_ids(w, {pointer}, {count})"),
    }
}

fn emit_struct(api: &Api, name: &str, out: &mut String) {
    let members = api.registry.struct_members(name).expect("structure");
    writeln!(
        out,
        "#[inline]\npub unsafe fn enc_{name}<W: Sink>(w: &mut W, src: *const {name}) -> i64 {{\n    let at = w.alloc(size_of::<{name}>());\n    w.copy(at, src.cast(), size_of::<{name}>());\n    fix_{name}(w, at, src);\n    at\n}}\n"
    )
    .ok();
    writeln!(
        out,
        "pub unsafe fn enc_n_{name}<W: Sink>(w: &mut W, src: *const {name}, n: usize) -> i64 {{\n    let at = w.alloc(size_of::<{name}>() * n);\n    w.copy(at, src.cast(), size_of::<{name}>() * n);\n    for i in 0..n {{\n        fix_{name}(w, at + (i * size_of::<{name}>()) as i64, src.add(i));\n    }}\n    at\n}}\n"
    )
    .ok();
    writeln!(out, "pub unsafe fn fix_{name}<W: Sink>(w: &mut W, at: i64, src: *const {name}) {{").ok();
    for member in members {
        let field = ident(&member.name);
        let place = format!("at + offset_of!({name}, {field}) as i64");
        let elements: String = member.array.iter().map(|d| format!("({})", array_length(d))).collect::<Vec<_>>().join(" * ");
        match api.member_kind(name, member) {
            MemberKind::PNext => {
                writeln!(out, "    let next = enc_chain(w, (*src).pNext);\n    w.put({place}, next);").ok();
            }
            MemberKind::Dispatchable if member.array.is_empty() => {
                writeln!(out, "    w.put({place}, rt::id((*src).{field}));").ok();
            }
            MemberKind::Fd => {
                // -1 is a valid "already signaled" sync file and travels as is.
                writeln!(out, "    if (*src).{field} >= 0 {{ w.send_fd((*src).{field}); }}").ok();
            }
            MemberKind::Pointer(pointee, len) => {
                let len = len.map(|l| length_expression(&l, &|id| format!("(*src).{}", ident(id))));
                let encode = encode_pointee(api, &pointee, &format!("(*src).{field}"), len.as_deref());
                writeln!(
                    out,
                    "    let data = if (*src).{field}.is_null() {{ 0 }} else {{ {encode} }};\n    w.put({place}, data);"
                )
                .ok();
            }
            MemberKind::Nested(inner) if api.needs_client(&inner) => {
                if member.array.is_empty() {
                    writeln!(out, "    fix_{inner}(w, {place}, &raw const (*src).{field});").ok();
                } else {
                    writeln!(
                        out,
                        "    for i in 0..{elements} {{\n        fix_{inner}(w, {place} + (i * size_of::<{inner}>()) as i64, (&raw const (*src).{field}).cast::<{inner}>().add(i));\n    }}"
                    )
                    .ok();
                }
            }
            MemberKind::Scratch(ty, len) => {
                let count = len.map_or_else(|| "1".to_owned(), |l| length_expression(&l, &|id| format!("(*src).{}", ident(id))));
                writeln!(
                    out,
                    "    let data = if (*src).{field}.is_null() {{ 0 }} else {{ w.alloc(size_of::<{ty}>() * {count}) }};\n    w.put({place}, data);"
                )
                .ok();
            }
            MemberKind::Unsupported => {
                writeln!(out, "    w.put({place}, 0usize);").ok();
            }
            MemberKind::Dispatchable => {
                writeln!(
                    out,
                    "    for i in 0..{elements} {{\n        w.put({place} + (i * 8) as i64, rt::id(*(&raw const (*src).{field}).cast::<usize>().add(i)));\n    }}"
                )
                .ok();
            }
            _ => {}
        }
    }
    out.push_str("}\n\n");
}

fn emit_chains(api: &Api, out: &mut String) {
    let chained = api.chained();
    out.push_str("/// Encode the first known structure of an input chain; unknown ones are skipped.\npub unsafe fn enc_chain<W: Sink>(w: &mut W, mut p: *const c_void) -> i64 {\n    while !p.is_null() {\n        let base = p.cast::<rt::Base>();\n        match (*base).sType {\n");
    for (name, value) in &chained {
        writeln!(out, "            {value} => return enc_{name}(w, p.cast()),").ok();
    }
    out.push_str("            other => rt::skipped(other),\n        }\n        p = (*base).pNext;\n    }\n    0\n}\n\n");

    out.push_str("/// Lay out empty copies of an output chain's known structures in the reply area.\npub unsafe fn skel_chain<W: Sink>(w: &mut W, mut p: *const c_void) -> i64 {\n    while !p.is_null() {\n        let base = p.cast::<rt::Base>();\n        let size = match (*base).sType {\n");
    for (name, value) in &chained {
        writeln!(out, "            {value} => size_of::<{name}>(),").ok();
    }
    out.push_str("            other => { rt::skipped(other); 0 }\n        };\n        if size != 0 {\n            let at = w.reply_alloc(size);\n            w.put(at, (*base).sType);\n            skel_arrays(w, p, at);\n            let next = skel_chain(w, (*base).pNext);\n            w.put(at + 8, next);\n            return at;\n        }\n        p = (*base).pNext;\n    }\n    0\n}\n\n");

    let arrays = api.output_arrays();
    out.push_str("/// Reserve the array an output structure's caller provides, right after its\n/// skeleton: the count travels as the capacity.\nunsafe fn skel_arrays<W: Sink>(w: &mut W, p: *const c_void, at: i64) {\n    match (*p.cast::<rt::Base>()).sType {\n");
    for (name, value, count, array, element) in &arrays {
        let (count, array) = (ident(count), ident(array));
        writeln!(
            out,
            "        {value} => {{\n            let src = p.cast::<{name}>();\n            let capacity = if (*src).{array}.is_null() {{ 0 }} else {{ (*src).{count} }};\n            w.put(at + offset_of!({name}, {count}) as i64, capacity);\n            let data = if capacity == 0 {{ 0 }} else {{ w.reply_alloc(size_of::<{element}>() * capacity as usize) }};\n            w.put(at + offset_of!({name}, {array}) as i64, data);\n        }}"
        )
        .ok();
    }
    out.push_str("        _ => {}\n    }\n}\n\n");

    out.push_str("/// Copy the host's answers from a reply skeleton back into the caller's chain.\n/// Returns the reply position after the chain.\npub unsafe fn copy_back_chain(mut p: *mut c_void, mut reply: *const u8) -> *const u8 {\n    while !p.is_null() {\n        let base = p.cast::<rt::Base>();\n        let size = match (*base).sType {\n");
    for (name, value) in &chained {
        writeln!(out, "            {value} => size_of::<{name}>(),").ok();
    }
    out.push_str("            _ => 0,\n        };\n        if size != 0 {\n            match (*base).sType {\n");
    for (name, value, count, array, element) in &arrays {
        let (count, array) = (ident(count), ident(array));
        writeln!(
            out,
            "                {value} => {{\n                    let dst = p.cast::<{name}>();\n                    let data = (*dst).{array};\n                    let capacity = if data.is_null() {{ 0 }} else {{ (*dst).{count} as usize }};\n                    core::ptr::copy_nonoverlapping(reply.add(16), p.cast::<u8>().add(16), size - 16);\n                    (*dst).{array} = data;\n                    reply = reply.add(rt::align(size));\n                    if capacity != 0 {{\n                        core::ptr::copy_nonoverlapping(reply, data.cast::<u8>(), size_of::<{element}>() * ((*dst).{count} as usize).min(capacity));\n                        reply = reply.add(rt::align(size_of::<{element}>() * capacity));\n                    }}\n                }}"
        )
        .ok();
    }
    out.push_str("                _ => {\n                    core::ptr::copy_nonoverlapping(reply.add(16), p.cast::<u8>().add(16), size - 16);\n                    reply = reply.add(rt::align(size));\n                }\n            }\n        }\n        p = (*base).pNext.cast_mut();\n    }\n    reply\n}\n\n");
}

fn param_field(cmd: &Cmd, id: &str) -> String {
    let param = cmd.params.iter().find(|p| p.member.name == id).unwrap_or_else(|| panic!("{}: no parameter {id}", cmd.name));
    if matches!(param.kind, ParamKind::Out { .. }) { format!("(*{})", ident(id)) } else { ident(id) }
}

fn emit_command(api: &Api, cmd: &Cmd, out: &mut String) {
    let name = &cmd.name;
    let signature = pfn_signature(api, cmd);
    let params_decl = signature
        .trim_start_matches("unsafe extern \"system\" fn(")
        .rsplit_once(')')
        .map(|(params, _)| params.to_owned())
        .unwrap_or_default();
    let ret = if cmd.ret == "void" { String::new() } else { format!(" -> {}", type_name(api, &cmd.ret)) };
    let args: Vec<String> = cmd.params.iter().map(|p| ident(&p.member.name)).collect();
    let outs = cmd.params.iter().filter(|p| matches!(p.kind, ParamKind::Out { .. } | ParamKind::FdOut)).count() + 1;
    let frame_at = |field: &str| format!("8 + offset_of!(frame::{name}, {field}) as i64");

    // The body, shared by the measuring and writing passes.
    writeln!(
        out,
        "#[inline(always)]\npub unsafe fn body_{name}<W: Sink>(w: &mut W, ids: &[u64], outs: &mut [i64; {outs}], {params_decl}) {{\n    let _ = (&ids, &outs);"
    )
    .ok();
    let mut created = 0;
    let mut out_index = 1;
    if cmd.sync {
        writeln!(out, "    outs[0] = w.reply_alloc(8);\n    w.put({}, outs[0]);", frame_at("reply")).ok();
    }
    for p in &cmd.params {
        let field = ident(&p.member.name);
        let len = |l: &Option<String>| l.as_ref().map(|l| length_expression(l, &|id| param_field(cmd, id)));
        match &p.kind {
            ParamKind::Value | ParamKind::Array(_) => {
                writeln!(out, "    w.put({}, {});", frame_at(&field), if matches!(p.kind, ParamKind::Array(_)) { format!("*{field}.cast::<{}>()", frame_type(api, &p.kind, &p.member).expect("array type")) } else { field.clone() }).ok();
            }
            ParamKind::FdIn => {
                writeln!(out, "    w.lend_fd({field});").ok();
            }
            ParamKind::Handle => {
                writeln!(out, "    w.put({}, {field});", frame_at(&field)).ok();
            }
            ParamKind::Dispatchable => {
                writeln!(out, "    w.put({}, rt::id({field}));", frame_at(&field)).ok();
            }
            ParamKind::In(pointee, l) => {
                let encode = encode_pointee(api, pointee, &field, len(l).as_deref());
                writeln!(out, "    let wire = if {field}.is_null() {{ 0 }} else {{ {encode} }};\n    w.put({}, wire);", frame_at(&field)).ok();
            }
            ParamKind::Created { len: None, .. } => {
                writeln!(out, "    w.put({}, ids[{created}]);", frame_at(&field)).ok();
                created += 1;
            }
            ParamKind::Created { len: Some(_), .. } => {
                writeln!(out, "    let wire = rt::enc_pod::<u64, W>(w, ids.as_ptr(), ids.len());\n    w.put({}, wire);", frame_at(&field)).ok();
            }
            ParamKind::FdOut => {
                writeln!(out, "    outs[{out_index}] = w.reply_alloc(8);\n    w.put({}, outs[{out_index}]);", frame_at(&field)).ok();
                out_index += 1;
            }
            ParamKind::Out { pointee, len: l, inout } => {
                let ty = pointee_type(api, pointee);
                let count = len(l).unwrap_or_else(|| "1".into());
                writeln!(out, "    outs[{out_index}] = if {field}.is_null() {{ 0 }} else {{ w.reply_alloc(size_of::<{ty}>() * {count}) }};\n    w.put({}, outs[{out_index}]);", frame_at(&field)).ok();
                if *inout {
                    writeln!(out, "    if !{field}.is_null() {{ w.put(outs[{out_index}], *{field}); }}").ok();
                }
                if let Pointee::Struct(_) = pointee {
                    writeln!(
                        out,
                        "    if !{field}.is_null() {{\n        for i in 0..{count} {{\n            let at = outs[{out_index}] + (i * size_of::<{ty}>()) as i64;\n            w.put(at, (*{field}.add(i)).sType);\n            let next = skel_chain(w, (*{field}.add(i)).pNext);\n            w.put(at + 8, next);\n        }}\n    }}"
                    )
                    .ok();
                }
                out_index += 1;
            }
            ParamKind::StreamOwner | ParamKind::Allocator => {}
        }
    }
    out.push_str("}\n\n");

    if cmd.client_manual {
        return;
    }

    // The entry point.
    writeln!(out, "pub unsafe extern \"system\" fn {name}({params_decl}){ret} {{").ok();
    let destroyed = cmd.destroys.map(|index| &cmd.params[index]);
    if let Some(param) = destroyed {
        if matches!(param.kind, ParamKind::Handle | ParamKind::Dispatchable) {
            let early = if cmd.ret == "VkResult" { "return VK_SUCCESS;" } else { "return;" };
            writeln!(out, "    if {} as u64 == 0 {{ {early} }}", ident(&param.member.name)).ok();
        }
    }
    let created: Vec<_> = cmd.params.iter().filter(|p| matches!(p.kind, ParamKind::Created { .. })).collect();
    match created.as_slice() {
        [] => out.push_str("    let ids: [u64; 0] = [];\n"),
        [param] => match &param.kind {
            ParamKind::Created { len: None, .. } => out.push_str("    let ids = [rt::new_id()];\n"),
            ParamKind::Created { len: Some(l), .. } => {
                let count = length_expression(l, &|id| param_field(cmd, id));
                writeln!(out, "    let ids: Vec<u64> = (0..{count}).map(|_| rt::new_id()).collect();").ok();
            }
            _ => unreachable!(),
        },
        _ => panic!("{name}: several created outputs"),
    }
    writeln!(out, "    let mut outs = [0i64; {outs}];").ok();
    let fixed = cmd.params.iter().all(|p| {
        matches!(p.kind, ParamKind::Value | ParamKind::Array(_) | ParamKind::Handle | ParamKind::Dispatchable | ParamKind::StreamOwner | ParamKind::Allocator | ParamKind::Created { len: None, .. })
    });
    let frame = format!("8 + size_of::<frame::{name}>()");
    if fixed && !cmd.sync {
        writeln!(out, "    let record_size = {frame};").ok();
    } else {
        writeln!(out, "    let mut m = rt::Measure::new({frame});\n    body_{name}(&mut m, &ids, &mut outs, {});\n    let record_size = m.size();", args.join(", ")).ok();
    }
    let fd_out = cmd.params.iter().any(|p| matches!(p.kind, ParamKind::FdOut));
    if cmd.stream {
        writeln!(out, "    let rec = rt::stream({}).reserve(op::{name}, record_size);", args[0]).ok();
        writeln!(out, "    let mut w = rt::Write::new(rec, {frame});\n    body_{name}(&mut w, &ids, &mut outs, {});", args.join(", ")).ok();
    } else {
        if fd_out {
            out.push_str("    let _fds = rt::fd_lock();\n");
        }
        writeln!(out, "    let mut ring = rt::ring();\n    let rec = ring.reserve(op::{name}, record_size);").ok();
        writeln!(out, "    let mut w = rt::Write::new(rec, {frame});\n    body_{name}(&mut w, &ids, &mut outs, {});", args.join(", ")).ok();
        if cmd.sync {
            out.push_str("    let end = ring.publish(true);\n    drop(ring);\n    rt::wait(end);\n");
        } else {
            out.push_str("    ring.publish(false);\n    drop(ring);\n");
        }
    }

    // Results back to the caller.
    let mut out_index = 1;
    for p in &cmd.params {
        let field = ident(&p.member.name);
        match &p.kind {
            ParamKind::Created { dispatchable, len } => {
                let make = if *dispatchable { "rt::new_dispatchable(id) as _" } else { "id" };
                if len.is_none() {
                    writeln!(out, "    let id = ids[0];\n    *{field} = {make};").ok();
                } else {
                    writeln!(out, "    for (i, &id) in ids.iter().enumerate() {{\n        *{field}.add(i) = {make};\n    }}").ok();
                }
            }
            ParamKind::FdOut => {
                writeln!(out, "    let value = *w.at::<c_int>(outs[{out_index}]);\n    *{field} = if value >= 0 {{ rt::recv_fd() }} else {{ value }};").ok();
                out_index += 1;
            }
            ParamKind::Out { pointee, len, .. } => {
                let ty = pointee_type(api, pointee);
                // Counts written by the host are read back before the arrays they size.
                let count = len.as_ref().map_or_else(|| "1".into(), |l| {
                    length_expression(l, &|id| {
                        let index = cmd.params.iter().position(|q| q.member.name == id).expect("count parameter");
                        let q = &cmd.params[index];
                        if !matches!(q.kind, ParamKind::Out { .. }) {
                            return ident(id);
                        }
                        let slot = 1 + cmd.params[..index].iter().filter(|q| matches!(q.kind, ParamKind::Out { .. } | ParamKind::FdOut)).count();
                        format!("(*w.at::<{}>(outs[{slot}]))", type_name(api, &q.member.ty))
                    })
                });
                writeln!(out, "    if !{field}.is_null() {{").ok();
                if let Pointee::Struct(_) = pointee {
                    writeln!(
                        out,
                        "        let n = {count};\n        let mut chain = w.at::<u8>(outs[{out_index}]).add(rt::align(size_of::<{ty}>() * n)).cast_const();\n        for i in 0..n {{\n            let reply = w.at::<u8>(outs[{out_index}]).add(i * size_of::<{ty}>());\n            core::ptr::copy_nonoverlapping(reply.add(16), {field}.add(i).cast::<u8>().add(16), size_of::<{ty}>() - 16);\n            chain = copy_back_chain((*{field}.add(i)).pNext, chain);\n        }}"
                    )
                    .ok();
                } else {
                    writeln!(out, "        core::ptr::copy_nonoverlapping(w.at::<{ty}>(outs[{out_index}]), {field}.cast::<{ty}>(), {count});").ok();
                }
                out.push_str("    }\n");
                out_index += 1;
            }
            _ => {}
        }
    }
    if let Some(param) = destroyed {
        let field = ident(&param.member.name);
        match &param.kind {
            ParamKind::Handle => writeln!(out, "    rt::free_id({field});").ok(),
            ParamKind::Dispatchable => writeln!(out, "    rt::free_dispatchable({field});").ok(),
            ParamKind::In(Pointee::Handles, Some(l)) => {
                let count = length_expression(l, &|id| param_field(cmd, id));
                writeln!(out, "    for i in 0..{count} {{ rt::free_id(*{field}.add(i)); }}").ok()
            }
            _ => None,
        };
    }
    if cmd.ret == "VkResult" {
        if cmd.sync {
            out.push_str("    *w.at::<VkResult>(outs[0])\n");
        } else {
            out.push_str("    VK_SUCCESS\n");
        }
    } else if cmd.ret != "void" {
        writeln!(out, "    *w.at::<{}>(outs[0])", type_name(api, &cmd.ret)).ok();
    }
    out.push_str("}\n\n");
}

pub fn emit(api: &Api) -> String {
    let mut out = String::new();
    writeln!(out, "// @generated by droidloom-gpu-gen from vk.xml {}. Do not edit.", api.registry.header_version).ok();
    out.push_str("#![allow(non_snake_case, dead_code, unused_mut, unused_variables, clippy::all, clippy::pedantic)]\n\nuse core::mem::{offset_of, size_of};\n\nuse droidloom_gpu_protocol::generated::{frame, op};\nuse droidloom_gpu_protocol::generated::vk::*;\n\nuse crate::rt::{self, Sink};\n\n");
    for name in &api.structs {
        if api.needs_client(name) {
            emit_struct(api, name, &mut out);
        }
    }
    emit_chains(api, &mut out);
    for cmd in &api.commands {
        emit_command(api, cmd, &mut out);
    }

    // Core commands that must not be null: calling one names it and aborts.
    out.push_str("/// Core commands not forwarded yet. Arguments are ignored: the call ends here.\nmod missing {\n    use super::rt;\n");
    for (name, _) in &api.missing {
        for name in std::iter::once(name).chain(api.registry.command_aliases.get(name).into_iter().flatten()) {
            writeln!(out, "    pub unsafe extern \"system\" fn {name}() {{ rt::fail(\"{name} is not forwarded yet\") }}").ok();
        }
    }
    out.push_str("}\n\n");
    // Name lookup for vkGet*ProcAddr; runs while applications load, not per call.
    out.push_str("/// The entry point and level of a forwarded command or alias.\npub fn proc_addr(name: &[u8]) -> Option<(*const c_void, rt::Level)> {\n    Some(match name {\n");
    for cmd in &api.commands {
        let path = if cmd.client_manual { format!("crate::manual::{}", cmd.name) } else { cmd.name.clone() };
        let level = match cmd.level {
            Level::Global => "Global",
            Level::Instance => "Instance",
            Level::Physical => "Physical",
            Level::Device => "Device",
        };
        for name in std::iter::once(&cmd.name).chain(&cmd.aliases) {
            writeln!(out, "        b\"{name}\" => ({path} as *const c_void, rt::Level::{level}),").ok();
        }
    }
    for (name, level) in &api.missing {
        let level = match level {
            Level::Global => "Global",
            Level::Instance => "Instance",
            Level::Physical => "Physical",
            Level::Device => "Device",
        };
        for name in std::iter::once(name).chain(api.registry.command_aliases.get(name).into_iter().flatten()) {
            writeln!(out, "        b\"{name}\" => (missing::{name} as *const c_void, rt::Level::{level}),").ok();
        }
    }
    for (name, level) in &api.local {
        let level = match level {
            Level::Global => "Global",
            Level::Instance => "Instance",
            Level::Physical => "Physical",
            Level::Device => "Device",
        };
        writeln!(out, "        b\"{name}\" => (crate::manual::{name} as *const c_void, rt::Level::{level}),").ok();
    }
    out.push_str("        _ => return None,\n    })\n}\n");
    out
}
