//! API documentation renderer (`toto doc`): one Markdown
//! document per module (one `.gzv` file) plus an index. Rendering
//! is syntactic (parse only, no checking): any file that parses
//! renders, even one that would not check.

use crate::ast::{ItemKind, Program};
use crate::fmt::{extern_signature, fn_signature, struct_signature};

/// Render one module as Markdown: `# <name>`, the module-doc
/// prose, then `## Functions` / `## Structs` sections with one
/// `###` per item (a `toto` signature fence plus the item prose).
/// `Fn`, foreign (`extern`), and `Struct` items render; `Import`,
/// `State`, and `Migrate` items are not API surface and are
/// skipped. Undocumented items still list their signature; doc
/// lines join verbatim, so a blank doc line is a paragraph break.
pub fn render_module(name: &str, prog: &Program) -> String {
    let mut out = String::from("# ");
    out.push_str(name);
    out.push('\n');
    if !prog.doc.is_empty() {
        out.push('\n');
        out.push_str(&prog.doc.join("\n"));
        out.push('\n');
    }
    let mut fns: Vec<(String, &Vec<String>)> = Vec::new();
    let mut structs: Vec<(String, &Vec<String>)> = Vec::new();
    for item in &prog.items {
        match &item.kind {
            ItemKind::Fn(d) => fns.push((fn_signature(d), &item.doc)),
            ItemKind::Struct(d) => structs.push((struct_signature(d), &item.doc)),
            ItemKind::Extern(b) => {
                for d in &b.decls {
                    fns.push((extern_signature(d), &item.doc));
                }
            }
            ItemKind::Import(_) | ItemKind::State(_) | ItemKind::Migrate(_) => {}
        }
    }
    render_section(&mut out, "Functions", &fns);
    render_section(&mut out, "Structs", &structs);
    out
}

fn render_section(out: &mut String, title: &str, items: &[(String, &Vec<String>)]) {
    if items.is_empty() {
        return;
    }
    out.push('\n');
    out.push_str("## ");
    out.push_str(title);
    out.push('\n');
    for (sig, doc) in items {
        out.push('\n');
        out.push_str("### ");
        out.push_str(item_name(sig));
        out.push_str("\n\n```toto\n");
        out.push_str(sig);
        out.push_str("\n```\n");
        if !doc.is_empty() {
            out.push('\n');
            out.push_str(&doc.join("\n"));
            out.push('\n');
        }
    }
}

/// Bare item name from a one-line signature: the word after `fn `
/// or `struct `, cut at the first `(`/space/`{`.
fn item_name(sig: &str) -> &str {
    let s = sig
        .strip_prefix("fn ")
        .or_else(|| sig.strip_prefix("struct "))
        .unwrap_or(sig);
    s.split(['(', ' ', '{']).next().unwrap_or(s)
}

/// Render the module index: `# Index` plus one bullet per module,
/// `- [name](target)` with ` - summary` when the module has a
/// first doc line. `target` maps a module name to its link target
/// (`math.md` for `--out` files mode, `#math` anchors for the
/// single-document stdout mode).
pub fn render_index(modules: &[(&str, &str)], target: &dyn Fn(&str) -> String) -> String {
    let mut out = String::from("# Index\n");
    for (name, summary) in modules {
        out.push('\n');
        out.push_str("- [");
        out.push_str(name);
        out.push_str("](");
        out.push_str(&target(name));
        out.push(')');
        if !summary.is_empty() {
            out.push_str(" - ");
            out.push_str(summary);
        }
        out.push('\n');
    }
    out
}
