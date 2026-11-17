//! AOT object emission for `toto build` (Phase 5, Stage 1).
//!
//! Mirrors the JIT task-mode driver over `ObjectModule`: the same
//! lowering core (`FnCx` through `Session`), the task-mode import set
//! with keys mapped to `task_*` link symbols (`#[unsafe(no_mangle)]`
//! in `task.rs`), user functions exported as `toto_fn_<name>`, static
//! bytes as module data (never baked addresses). The produced object
//! links against the `toto-jit` runtime helpers through a generated
//! shim crate; execution hosting is `run_aot` in `task.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use cranelift_codegen::ir::{InstBuilder, types::I64};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, isa::TargetFrontendConfig};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId as ClFuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use toto_compiler::hir::Ty;
use toto_compiler::mir::{MProgram, MirBody};

use super::compile::{
    BodyKind, BodyShape, CodegenMode, FnCx, OptLevel, Session, SubsetError, body_fn_name,
    declare_runtime_imports, sig_arity,
};

/// User-function link symbol: prefixed so a Toto `fn main` never
/// collides with the host `main` (Toto identifiers cannot spell the
/// bodies' `$` suffix, and every user symbol carries this prefix).
fn fn_symbol(name: &str) -> String {
    format!("toto_fn_{name}")
}

/// Map a `toto_*` import key to its link symbol. Every helper follows
/// the `toto_X` → `task_X` rule except the capture helper, whose Rust
/// name drops the stutter (`task_capture`).
pub(crate) fn aot_symbol(key: &str) -> String {
    match key {
        "toto_task_capture" => "task_capture".to_string(),
        rest => format!("task_{}", &rest["toto_".len()..]),
    }
}

/// Object-module codegen session (the AOT half of `Session`).
pub(crate) struct ObjSession {
    module: ObjectModule,
    imports: HashMap<&'static str, ClFuncId>,
    funcs: HashMap<String, ClFuncId>,
    bodies: Vec<(u32, BodyKind, Arc<MirBody>)>,
    next_body: u32,
    chan_types: Vec<Ty>,
    data: HashMap<String, DataId>,
    timings: Vec<(String, u64)>,
}

impl ObjSession {
    fn isa_config(&self) -> TargetFrontendConfig {
        self.module.isa().frontend_config()
    }
}

impl Session for ObjSession {
    fn mode(&self) -> CodegenMode {
        CodegenMode::Task
    }

    fn import(&mut self, name: &'static str, params: u32, returns: bool) -> ClFuncId {
        if let Some(id) = self.imports.get(name) {
            return *id;
        }
        let sig = sig_arity(params, returns);
        let id = self
            .module
            .declare_function(&aot_symbol(name), Linkage::Import, &sig)
            .expect("declare import");
        self.imports.insert(name, id);
        id
    }

    fn import_id(&self, name: &'static str) -> Option<ClFuncId> {
        self.imports.get(name).copied()
    }

    fn intern_str(&mut self, b: &mut FunctionBuilder, s: &str) -> (CVal, CVal) {
        if !self.data.contains_key(s) {
            let id = self
                .module
                .declare_data(
                    &format!("toto_str{}", self.data.len()),
                    Linkage::Local,
                    false,
                    false,
                )
                .expect("declare static data");
            let mut desc = DataDescription::new();
            desc.define(s.as_bytes().to_vec().into_boxed_slice());
            self.module
                .define_data(id, &desc)
                .expect("define static data");
            self.data.insert(s.to_string(), id);
        }
        let id = self.data[s];
        let g = self.module.declare_data_in_func(id, b.func);
        let ptr = b.ins().symbol_value(I64, g);
        let len = b.ins().iconst(I64, s.len() as i64);
        (ptr, len)
    }

    fn declare_in_func(
        &mut self,
        id: ClFuncId,
        func: &mut cranelift_codegen::ir::Function,
    ) -> cranelift_codegen::ir::FuncRef {
        self.module.declare_func_in_func(id, func)
    }

    fn func_id(&self, name: &str) -> Option<ClFuncId> {
        self.funcs.get(name).copied()
    }

    fn alloc_body(&mut self, kind: BodyKind, body: Arc<MirBody>) -> u32 {
        let id = self.next_body;
        self.next_body += 1;
        self.bodies.push((id, kind, body));
        id
    }

    fn intern_chan_type(&mut self, elem: Ty) -> u64 {
        let id = self.chan_types.len() as u64;
        self.chan_types.push(elem);
        id
    }
}

type CVal = cranelift_codegen::ir::Value;

/// What `toto build` needs beyond the object bytes: link symbols for
/// the generated shim plus the host-side tables the runner consumes.
#[derive(Clone, Debug)]
pub struct AotArtifact {
    /// The linked object file bytes (ELF, PIC).
    pub object: Vec<u8>,
    /// `(Toto name, link symbol)` for every user function.
    pub fn_symbols: Vec<(String, String)>,
    /// `(body id, link symbol)` for every nested body.
    pub body_symbols: Vec<(u32, String)>,
    /// Interned channel element types, in id order.
    pub chan_types: Vec<Ty>,
    /// User function names in definition order.
    pub func_names: Vec<String>,
    /// Per-function codegen times `(name, microseconds)`.
    pub timings: Vec<(String, u64)>,
}

/// Compile a real MIR program to an object file (task-mode semantics:
/// concurrent programs supported). Fails only on subset violations or
/// backend errors — checked programs always pass.
pub fn compile_to_object(
    prog: &MProgram,
    opt: OptLevel,
    name: &str,
) -> Result<AotArtifact, String> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", opt.flag())
        .map_err(|e| format!("opt_level flag: {e}"))?;
    // Position-independent code: the object links into a (PIE) binary.
    flag_builder
        .set("is_pic", "true")
        .map_err(|e| format!("is_pic flag: {e}"))?;
    let flags = settings::Flags::new(flag_builder);
    let isa = cranelift_native::builder()
        .map_err(|e| e.to_string())?
        .finish(flags)
        .map_err(|e| e.to_string())?;
    let builder = ObjectBuilder::new(
        isa,
        name.as_bytes().to_vec(),
        cranelift_module::default_libcall_names(),
    )
    .map_err(|e| format!("object builder: {e}"))?;
    let mut sess = ObjSession {
        module: ObjectModule::new(builder),
        imports: HashMap::new(),
        funcs: HashMap::new(),
        bodies: Vec::new(),
        next_body: 0,
        chan_types: Vec::new(),
        data: HashMap::new(),
        timings: Vec::new(),
    };
    declare_runtime_imports(&mut sess, true);
    for f in &prog.fns {
        let symbol = fn_symbol(&f.name);
        let sig = sig_arity(f.params.len() as u32, true);
        let id = sess
            .module
            .declare_function(&symbol, Linkage::Export, &sig)
            .map_err(|e| SubsetError {
                func: f.name.clone(),
                message: format!("declare failed: {e}"),
            })
            .map_err(|e| e.to_string())?;
        sess.funcs.insert(f.name.clone(), id);
    }
    for fi in 0..prog.fns.len() {
        define_top(&mut sess, prog, fi).map_err(|e| e.to_string())?;
    }
    let mut done = 0usize;
    while done < sess.bodies.len() {
        let pending: Vec<(u32, BodyKind, Arc<MirBody>)> = sess.bodies[done..].to_vec();
        for (id, kind, body) in &pending {
            let nparams = match kind {
                BodyKind::Closure => body.params.len() + 1,
                BodyKind::Go => 0,
                BodyKind::TopLevel => {
                    return Err(format!(
                        "top-level body in the body table (unreachable): {}",
                        body_fn_name(*id)
                    ));
                }
            };
            let name = body_fn_name(*id);
            let sig = sig_arity(nparams as u32, true);
            let cl_id = sess
                .module
                .declare_function(&name, Linkage::Export, &sig)
                .map_err(|e| format!("declare failed: {e}"))?;
            sess.funcs.insert(name, cl_id);
        }
        for (id, kind, body) in &pending {
            define_body(&mut sess, prog, *id, *kind, body).map_err(|e| e.to_string())?;
        }
        done += pending.len();
    }
    let object = sess
        .module
        .finish()
        .emit()
        .map_err(|e| format!("object emit: {e}"))?;
    let fn_symbols = prog
        .fns
        .iter()
        .map(|f| (f.name.clone(), fn_symbol(&f.name)))
        .collect();
    let body_symbols = sess
        .bodies
        .iter()
        .map(|(id, _, _)| (*id, body_fn_name(*id)))
        .collect();
    Ok(AotArtifact {
        object,
        fn_symbols,
        body_symbols,
        chan_types: std::mem::take(&mut sess.chan_types),
        func_names: prog.fns.iter().map(|f| f.name.clone()).collect(),
        timings: std::mem::take(&mut sess.timings),
    })
}

fn define_top(sess: &mut ObjSession, prog: &MProgram, fi: usize) -> Result<(), SubsetError> {
    let f = &prog.fns[fi];
    let cl_id = sess.funcs[&f.name];
    let mut ctx = Context::new();
    ctx.func.signature = sig_arity(f.params.len() as u32, true);
    let mut fctx = FunctionBuilderContext::new();
    let shape = BodyShape {
        name: &f.name,
        params: &f.params,
        ret: &f.ret,
        locals: &f.locals,
        blocks: &f.blocks,
        entry: f.entry,
        body_span: f.body_span,
        kind: BodyKind::TopLevel,
        ncaptures: 0,
    };
    define_common(sess, &f.name, cl_id, prog, shape, &mut ctx, &mut fctx)
}

fn define_body(
    sess: &mut ObjSession,
    prog: &MProgram,
    id: u32,
    kind: BodyKind,
    body: &MirBody,
) -> Result<(), SubsetError> {
    let name = body_fn_name(id);
    let cl_id = sess.funcs[&name];
    let nparams = match kind {
        BodyKind::Closure => body.params.len() + 1,
        BodyKind::Go => 0,
        BodyKind::TopLevel => {
            return Err(SubsetError {
                func: name.clone(),
                message: String::from("top-level body in the body table (unreachable)"),
            });
        }
    };
    let mut ctx = Context::new();
    ctx.func.signature = sig_arity(nparams as u32, true);
    let mut fctx = FunctionBuilderContext::new();
    let shape = BodyShape {
        name: &name,
        params: &body.params,
        ret: &body.ret,
        locals: &body.locals,
        blocks: &body.blocks,
        entry: body.entry,
        body_span: body.body_span,
        kind,
        ncaptures: body.captures.len(),
    };
    define_common(sess, &name, cl_id, prog, shape, &mut ctx, &mut fctx)
}

/// Lower one body behind the totality seatbelt, define it, and record
/// its codegen time (mirrors the JIT driver).
fn define_common(
    sess: &mut ObjSession,
    name: &str,
    cl_id: ClFuncId,
    prog: &MProgram,
    shape: BodyShape<'_>,
    ctx: &mut Context,
    fctx: &mut FunctionBuilderContext,
) -> Result<(), SubsetError> {
    let t0 = std::time::Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut b = FunctionBuilder::new(&mut ctx.func, fctx);
        FnCx::lower(sess, &mut b, prog, shape)?;
        let frontend_config = sess.isa_config();
        b.finalize(frontend_config);
        Ok(())
    }));
    let r = match r {
        Ok(r) => r,
        Err(_) => Err(SubsetError {
            func: name.to_string(),
            message: String::from("invalid MIR input (use before definition?)"),
        }),
    };
    r?;
    sess.module
        .define_function(cl_id, ctx)
        .map_err(|e| SubsetError {
            func: name.to_string(),
            message: format!("define failed: {e}"),
        })?;
    let dt = t0.elapsed().as_micros() as u64;
    sess.timings.push((name.to_string(), dt));
    Ok(())
}

/// Render a channel element type as a Rust expression (for the shim's
/// `chan_types` table). The shim links `toto-compiler`, so the paths
/// below resolve there.
pub fn ty_to_rust_expr(ty: &Ty) -> String {
    match ty {
        Ty::Int => "toto_compiler::hir::Ty::Int".to_string(),
        Ty::Float => "toto_compiler::hir::Ty::Float".to_string(),
        Ty::String => "toto_compiler::hir::Ty::String".to_string(),
        Ty::Bool => "toto_compiler::hir::Ty::Bool".to_string(),
        Ty::Unit => "toto_compiler::hir::Ty::Unit".to_string(),
        Ty::Range => "toto_compiler::hir::Ty::Range".to_string(),
        Ty::Array(t) => format!(
            "toto_compiler::hir::Ty::Array(Box::new({}))",
            ty_to_rust_expr(t)
        ),
        Ty::Map(k, v) => format!(
            "toto_compiler::hir::Ty::Map(Box::new({}), Box::new({}))",
            ty_to_rust_expr(k),
            ty_to_rust_expr(v)
        ),
        Ty::Result(t, e) => format!(
            "toto_compiler::hir::Ty::Result(Box::new({}), Box::new({}))",
            ty_to_rust_expr(t),
            ty_to_rust_expr(e)
        ),
        Ty::Option(t) => format!(
            "toto_compiler::hir::Ty::Option(Box::new({}))",
            ty_to_rust_expr(t)
        ),
        Ty::Chan(t) => format!(
            "toto_compiler::hir::Ty::Chan(Box::new({}))",
            ty_to_rust_expr(t)
        ),
        Ty::Struct(n) => format!("toto_compiler::hir::Ty::Struct({n:?})"),
        // C scalars render as their Toto views (values genuinely are
        // `int`/`float`; only pointers are distinct, and they nest).
        // Unreachable in practice: `toto build` refuses FFI programs
        // before lowering, and `chan<C>` is rejected statically.
        Ty::CChar | Ty::CInt | Ty::CLong => "toto_compiler::hir::Ty::Int".to_string(),
        Ty::CDouble => "toto_compiler::hir::Ty::Float".to_string(),
        Ty::CVoid => "toto_compiler::hir::Ty::Unit".to_string(),
        Ty::CPtr(t) => format!(
            "toto_compiler::hir::Ty::CPtr(Box::new({}))",
            ty_to_rust_expr(t)
        ),
        Ty::Fn(params, ret) => {
            let ps = params
                .iter()
                .map(ty_to_rust_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "toto_compiler::hir::Ty::Fn(vec![{ps}], Box::new({}))",
                ty_to_rust_expr(ret)
            )
        }
    }
}
