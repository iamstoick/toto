//! Toy IR -> CLIF lowering + `cranelift-jit` session.
//!
//! Signatures: every toy function is `(ctx: i64, args...) -> i64`
//! (ctx first — the internal-ABI rule under test). Runtime helpers
//! are declared as imports and resolved from host function pointers
//! registered on the `JITBuilder`. JIT-to-JIT calls use direct
//! `FuncRef`s; recursion works because all functions are declared
//! before any body is defined.

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, Signature, types::*};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClFuncId, Linkage, Module};

use crate::runtime::*;
use crate::toy::{Expr, FuncId};

fn sig_arity(params: u32, returns: bool) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(I64)); // ctx
    for _ in 0..params {
        sig.params.push(AbiParam::new(I64));
    }
    if returns {
        sig.returns.push(AbiParam::new(I64));
    }
    sig
}

#[derive(Clone, Copy)]
struct Import {
    id: ClFuncId,
}

pub struct Jit {
    module: JITModule,
    frontend_config: TargetFrontendConfig,
    imports: HashMap<&'static str, Import>,
    funcs: Vec<ClFuncId>,
    arities: Vec<u32>,
}

impl Jit {
    pub fn new() -> Result<Self, String> {
        Self::with_symbols()
    }

    fn with_symbols() -> Result<Self, String> {
        let mut jb = JITBuilder::new(cranelift_module::default_libcall_names())
            .map_err(|e| e.to_string())?;
        jb.symbol("toto_alloc_int", toto_alloc_int as *const u8);
        jb.symbol("toto_alloc_pair", toto_alloc_pair as *const u8);
        jb.symbol("toto_shadow_push", toto_shadow_push as *const u8);
        jb.symbol("toto_shadow_len", toto_shadow_len as *const u8);
        jb.symbol("toto_shadow_pop_to", toto_shadow_pop_to as *const u8);
        jb.symbol("toto_gc_collect", toto_gc_collect as *const u8);
        jb.symbol("toto_read_field", toto_read_field as *const u8);
        jb.symbol("toto_write_field", toto_write_field as *const u8);
        jb.symbol("toto_get_int_or", toto_get_int_or as *const u8);
        let module = JITModule::new(jb);
        // The frontend needs the target config at finalize time;
        // keep it alongside the module (same ISA the JIT uses).
        let frontend_config = module.isa().frontend_config();
        Ok(Self {
            module,
            frontend_config,
            imports: HashMap::new(),
            funcs: Vec::new(),
            arities: Vec::new(),
        })
    }

    fn import(&mut self, name: &'static str, params: u32, returns: bool) -> ClFuncId {
        if let Some(i) = self.imports.get(name) {
            return i.id;
        }
        let sig = sig_arity(params, returns);
        // Helper signatures exclude ctx duplication: params counts
        // NON-ctx args; ctx is always first.
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .expect("declare import");
        self.imports.insert(name, Import { id });
        id
    }

    fn declare_runtime(&mut self) {
        self.import("toto_alloc_int", 1, true);
        self.import("toto_alloc_pair", 2, true);
        self.import("toto_shadow_push", 1, false);
        self.import("toto_shadow_len", 0, true);
        self.import("toto_shadow_pop_to", 1, false);
        self.import("toto_gc_collect", 0, true);
        self.import("toto_read_field", 2, true);
        self.import("toto_write_field", 3, true);
        self.import("toto_get_int_or", 2, true);
    }

    /// Declare a toy function (callable by index afterwards, even
    /// before it is defined — enables recursion and recompile-by-new-symbol).
    pub fn declare(&mut self, name: &str, params: u32) -> FuncId {
        let sig = sig_arity(params, true);
        let id = self
            .module
            .declare_function(name, Linkage::Export, &sig)
            .expect("declare toy function");
        let idx = self.funcs.len() as FuncId;
        self.funcs.push(id);
        self.arities.push(params);
        idx
    }

    /// Define a previously declared function body and finalize.
    /// Finalize runs per definition batch; calling it repeatedly is
    /// supported (pending functions are finalized each time), which
    /// is what the recompile test relies on.
    pub fn define(&mut self, idx: FuncId, body: &Expr) {
        self.declare_runtime();
        let cl_id = self.funcs[idx as usize];
        let arity = self.arities[idx as usize];
        let mut ctx = Context::new();
        ctx.func.signature = sig_arity(arity, true);
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let params = b.block_params(entry).to_vec();
            let frontend_config = self.frontend_config;
            let mut vars = Vec::new();
            let lowered = lower_expr(self, &mut b, body, &params, None, &mut vars);
            b.ins().return_(&[lowered]);
            b.finalize(frontend_config);
        }
        self.module
            .define_function(cl_id, &mut ctx)
            .expect("define");
        self.module.finalize_definitions().expect("finalize");
    }

    pub fn ptr(&self, idx: FuncId) -> *const u8 {
        self.module.get_finalized_function(self.funcs[idx as usize])
    }

    /// Call a finalized function pointer with (ctx, args...).
    ///
    /// # Safety
    /// Pointer must come from `ptr`, ctx must be the live
    /// `JitRuntime`, arity must match, and i64 values crossing the
    /// boundary must be plain ints or encoded handles.
    pub unsafe fn call_ptr(ptr: *const u8, ctx: *mut JitRuntime, args: &[i64]) -> i64 {
        // Manual dispatch keeps each transmute target exact.
        // SAFETY: covered by the caller's contract (documented above).
        unsafe {
            match args {
                [] => {
                    let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(ptr);
                    f(ctx as i64)
                }
                [a0] => {
                    let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(ptr);
                    f(ctx as i64, *a0)
                }
                [a0, a1] => {
                    let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(ptr);
                    f(ctx as i64, *a0, *a1)
                }
                [a0, a1, a2] => {
                    let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                        std::mem::transmute(ptr);
                    f(ctx as i64, *a0, *a1, *a2)
                }
                _ => panic!("toy calls support up to 3 args"),
            }
        }
    }
}

fn runtime_call(
    jit: &mut Jit,
    b: &mut FunctionBuilder<'_>,
    name: &'static str,
    args: &[cranelift_codegen::ir::Value],
) -> Option<cranelift_codegen::ir::Value> {
    let import = *jit.imports.get(name).expect("import declared");
    let callee = jit.module.declare_func_in_func(import.id, b.func);
    let inst = b.ins().call(callee, args);
    let results = b.inst_results(inst).to_vec();
    results.into_iter().next()
}

fn toy_call(
    jit: &mut Jit,
    b: &mut FunctionBuilder<'_>,
    idx: FuncId,
    args: &[cranelift_codegen::ir::Value],
) -> cranelift_codegen::ir::Value {
    let cl_id = jit.funcs[idx as usize];
    let callee = jit.module.declare_func_in_func(cl_id, b.func);
    let inst = b.ins().call(callee, args);
    b.inst_results(inst)[0]
}

fn lower_expr(
    jit: &mut Jit,
    b: &mut FunctionBuilder<'_>,
    e: &Expr,
    params: &[cranelift_codegen::ir::Value],
    loop_index: Option<cranelift_codegen::ir::Value>,
    vars: &mut Vec<(u32, cranelift_codegen::ir::Value)>,
) -> cranelift_codegen::ir::Value {
    use cranelift_codegen::ir::condcodes::IntCC::*;
    match e {
        Expr::Lit(v) => b.ins().iconst(I64, *v),
        Expr::Arg(i) => params[*i as usize + 1],
        Expr::Index => loop_index.expect("Index outside Loop body"),
        Expr::Add(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            b.ins().iadd(x, y)
        }
        Expr::Sub(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            b.ins().isub(x, y)
        }
        Expr::Mul(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            b.ins().imul(x, y)
        }
        Expr::Div(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            b.ins().udiv(x, y)
        }
        Expr::Eq(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            let c = b.ins().icmp(Equal, x, y);
            b.ins().uextend(I64, c)
        }
        Expr::Lt(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            let c = b.ins().icmp(SignedLessThan, x, y);
            b.ins().uextend(I64, c)
        }
        Expr::IfElse { cond, then_, else_ } => {
            let c = lower_expr(jit, b, cond, params, loop_index, vars);
            let then_b = b.create_block();
            let else_b = b.create_block();
            let merge_b = b.create_block();
            b.append_block_param(merge_b, I64);
            b.ins().brif(c, then_b, &[], else_b, &[]);
            b.switch_to_block(then_b);
            b.seal_block(then_b);
            let v1 = lower_expr(jit, b, then_, params, loop_index, vars);
            b.ins().jump(merge_b, &[BlockArg::Value(v1)]);
            b.switch_to_block(else_b);
            b.seal_block(else_b);
            let v0 = lower_expr(jit, b, else_, params, loop_index, vars);
            b.ins().jump(merge_b, &[BlockArg::Value(v0)]);
            b.switch_to_block(merge_b);
            b.seal_block(merge_b);
            b.block_params(merge_b)[0]
        }
        Expr::Call(id, args) => {
            let mut vals = vec![params[0]];
            for a in args {
                vals.push(lower_expr(jit, b, a, params, loop_index, vars));
            }
            toy_call(jit, b, *id, &vals)
        }
        Expr::Loop { times, body } => {
            let n = lower_expr(jit, b, times, params, loop_index, vars);
            let header = b.create_block();
            let body_b = b.create_block();
            let exit = b.create_block();
            b.append_block_param(header, I64);
            b.append_block_param(header, I64);
            b.append_block_param(exit, I64);
            let zero = b.ins().iconst(I64, 0);
            b.ins()
                .jump(header, &[BlockArg::Value(zero), BlockArg::Value(zero)]);
            b.switch_to_block(header);
            let acc = b.block_params(header)[0];
            let i = b.block_params(header)[1];
            let go = b.ins().icmp(SignedLessThan, i, n);
            b.ins().brif(go, body_b, &[], exit, &[BlockArg::Value(acc)]);
            b.switch_to_block(body_b);
            let v = lower_expr(jit, b, body, params, Some(i), vars);
            let one = b.ins().iconst(I64, 1);
            let i1 = b.ins().iadd(i, one);
            b.ins()
                .jump(header, &[BlockArg::Value(v), BlockArg::Value(i1)]);
            b.switch_to_block(exit);
            b.seal_block(header);
            b.seal_block(body_b);
            b.seal_block(exit);
            b.block_params(exit)[0]
        }
        Expr::Seq(es) => {
            let mut last = None;
            for e in es {
                last = Some(lower_expr(jit, b, e, params, loop_index, vars));
            }
            last.unwrap_or_else(|| b.ins().iconst(I64, 0))
        }
        Expr::AllocInt(e) => {
            let v = lower_expr(jit, b, e, params, loop_index, vars);
            runtime_call(jit, b, "toto_alloc_int", &[params[0], v]).unwrap()
        }
        Expr::AllocPair(a, x) => {
            let (x, y) = (
                lower_expr(jit, b, a, params, loop_index, vars),
                lower_expr(jit, b, x, params, loop_index, vars),
            );
            runtime_call(jit, b, "toto_alloc_pair", &[params[0], x, y]).unwrap()
        }
        Expr::ReadField(o, idx) => {
            let o = lower_expr(jit, b, o, params, loop_index, vars);
            let i = b.ins().iconst(I64, *idx as i64);
            runtime_call(jit, b, "toto_read_field", &[params[0], o, i]).unwrap()
        }
        Expr::WriteField(o, idx, v) => {
            let o = lower_expr(jit, b, o, params, loop_index, vars);
            let i = b.ins().iconst(I64, *idx as i64);
            let v = lower_expr(jit, b, v, params, loop_index, vars);
            runtime_call(jit, b, "toto_write_field", &[params[0], o, i, v]).unwrap()
        }
        Expr::GetIntOr(o, d) => {
            let o = lower_expr(jit, b, o, params, loop_index, vars);
            let d = b.ins().iconst(I64, *d);
            runtime_call(jit, b, "toto_get_int_or", &[params[0], o, d]).unwrap()
        }
        Expr::Keep(e) => {
            let v = lower_expr(jit, b, e, params, loop_index, vars);
            runtime_call(jit, b, "toto_shadow_push", &[params[0], v]);
            v
        }
        Expr::GcCollect => runtime_call(jit, b, "toto_gc_collect", &[params[0]]).unwrap(),
        Expr::Let(name, val, body) => {
            let v = lower_expr(jit, b, val, params, loop_index, vars);
            vars.push((*name, v));
            let r = lower_expr(jit, b, body, params, loop_index, vars);
            vars.pop();
            r
        }
        Expr::Var(name) => vars
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
            .expect("Var bound by an enclosing Let"),
    }
}
