//! MIR-subset → Cranelift gate compiler.
//!
//! Compiles real `mir::lower` output for the Tier-2 gate. Every
//! generated function takes the runtime context first (frozen
//! language-level rule); every value traffics as `I64` (immediates as
//! bit patterns, heap objects as the ADR-008 handle codec).
//!
//! Supported subset (everything else is `SubsetError::Unsupported` —
//! the gate boundary, Phase 4 extends it):
//! - single-block functions ending in `Return` (no `Jump`/`Branch`/
//!   `FallOff`; control flow is Phase 4);
//! - `Const` (int/bool/float/unit/string; `None` refused — its `Ty` is
//!   `Option`, out of subset), `Copy`, `Bin` (int add/sub/mul with
//!   `E0201` checks, int div/mod with `DivCheck`-paired `E0202` checks
//!   plus `MIN/-1` handling exactly like `ops`, float add/sub/mul/div,
//!   int/float comparisons; float mod and all other mixes refused),
//!   `Neg` (int `E0201`, float), `Not` (bool), `DivCheck` (paired with
//!   the following div/mod; verified, not just trusted),
//! - `Call` to a statically known `FuncRef` target in the same module
//!   (indirect/closure calls refused), `CallBuiltin("len")` on arrays
//!   (all other builtins refused),
//! - `ArrayLit` (trafficable elements; lowered to `toto_alloc_array`
//!   plus per-element `toto_write_field`), `Index` (array by int with
//!   `E0209`), `Len` (array),
//! - `Safepoint` of any kind: publish all defined handle locals (the
//!   auto-generation rule); `Entry` additionally records the shadow
//!   base the epilogue pops to. Epilogues pop before every return, so
//!   a returned handle is unrooted at return by construction
//!   (documented; the caller resolves it before any collection).
//!
//! Root-publication discipline: before every call that can collect
//! (allocation/collection helpers and JIT-to-JIT calls — callees may
//! allocate), all currently defined handle-typed locals are pushed.
//! Conservative (no liveness analysis) but sound: the set published is
//! always a superset of the live handles.
//!
//! Panic paths never return: the check branches to a block calling
//! `toto_panic` (which diverges via the context handler) and then
//! returns a dummy value on the unreachable path — no trap
//! instruction is emitted anywhere, so no signal can escape generated
//! code by construction.

use std::collections::HashMap;
use std::fmt;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, Signature, types::*};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClFuncId, Linkage, Module};
use toto_compiler::hir::Ty;
use toto_compiler::mir::{MBinOp, MConst, MInstr, MProgram};

use crate::runtime::{E_DIV_ZERO, E_OVERFLOW, TAG_BOOL, TAG_FLOAT, TAG_HANDLE, TAG_INT, TAG_UNIT};

/// Gate boundary: MIR outside the supported subset.
#[derive(Clone, Debug)]
pub struct SubsetError {
    pub func: String,
    pub message: String,
}

impl fmt::Display for SubsetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gate subset: {}: {}", self.func, self.message)
    }
}

impl std::error::Error for SubsetError {}

/// Panic message bytes owned by the gate (addresses baked into
/// generated code as immediates; gate-only — Phase 4 uses rodata).
static MSG_DIV_ZERO: &str = "division by zero";
static MSG_OVERFLOW: &str = "integer overflow";

fn sig_arity(params: u32, returns: bool) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(I64)); // ctx first (frozen)
    for _ in 0..params {
        sig.params.push(AbiParam::new(I64));
    }
    if returns {
        sig.returns.push(AbiParam::new(I64));
    }
    sig
}

pub struct Jit {
    module: JITModule,
    imports: HashMap<&'static str, ClFuncId>,
    funcs: HashMap<String, ClFuncId>,
}

impl Jit {
    pub fn new() -> Result<Self, String> {
        let mut jb = JITBuilder::new(cranelift_module::default_libcall_names())
            .map_err(|e| e.to_string())?;
        // Frozen entry points used by the gate (each takes ctx first).
        jb.symbol(
            "toto_alloc_array",
            crate::runtime::toto_alloc_array as *const u8,
        );
        jb.symbol(
            "toto_write_field",
            crate::runtime::toto_write_field as *const u8,
        );
        jb.symbol(
            "toto_read_field",
            crate::runtime::toto_read_field as *const u8,
        );
        jb.symbol(
            "toto_array_len",
            crate::runtime::toto_array_len as *const u8,
        );
        jb.symbol(
            "toto_alloc_str",
            crate::runtime::toto_alloc_str as *const u8,
        );
        jb.symbol(
            "toto_shadow_push",
            crate::runtime::toto_shadow_push as *const u8,
        );
        jb.symbol(
            "toto_shadow_len",
            crate::runtime::toto_shadow_len as *const u8,
        );
        jb.symbol(
            "toto_shadow_pop_to",
            crate::runtime::toto_shadow_pop_to as *const u8,
        );
        jb.symbol(
            "toto_gc_collect",
            crate::runtime::toto_gc_collect as *const u8,
        );
        jb.symbol("toto_panic", crate::runtime::toto_panic as *const u8);
        jb.symbol("toto_yield", crate::runtime::toto_yield as *const u8);
        let module = JITModule::new(jb);
        Ok(Self {
            module,
            imports: HashMap::new(),
            funcs: HashMap::new(),
        })
    }

    fn import(&mut self, name: &'static str, params: u32, returns: bool) -> ClFuncId {
        if let Some(id) = self.imports.get(name) {
            return *id;
        }
        let sig = sig_arity(params, returns);
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .expect("declare import");
        self.imports.insert(name, id);
        id
    }

    fn declare_runtime(&mut self) {
        self.import("toto_alloc_array", 3, true);
        self.import("toto_write_field", 6, false);
        self.import("toto_read_field", 4, true);
        self.import("toto_array_len", 3, true);
        self.import("toto_alloc_str", 4, true);
        self.import("toto_shadow_push", 1, false);
        self.import("toto_shadow_len", 0, true);
        self.import("toto_shadow_pop_to", 1, false);
        self.import("toto_gc_collect", 0, true);
        self.import("toto_panic", 5, false);
        self.import("toto_yield", 0, true);
    }

    /// Compile every function in a real MIR program (two passes:
    /// declare all, then define all — recursion and any call order).
    pub fn compile(&mut self, prog: &MProgram) -> Result<(), SubsetError> {
        self.declare_runtime();
        for f in &prog.fns {
            let sig = sig_arity(f.params.len() as u32, true);
            let id = self
                .module
                .declare_function(&f.name, Linkage::Export, &sig)
                .map_err(|e| SubsetError {
                    func: f.name.clone(),
                    message: format!("declare failed: {e}"),
                })?;
            self.funcs.insert(f.name.clone(), id);
        }
        for f in &prog.fns {
            let cl_id = self.funcs[&f.name];
            let mut ctx = Context::new();
            ctx.func.signature = sig_arity(f.params.len() as u32, true);
            let mut fctx = FunctionBuilderContext::new();
            {
                let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let entry = b.create_block();
                b.append_block_params_for_function_params(entry);
                b.switch_to_block(entry);
                b.seal_block(entry);
                let params = b.block_params(entry).to_vec();
                let mut cx = FnCx::new(&f.name, params);
                cx.lower_fn(self, &mut b, prog, f)?;
                let frontend_config = self.module.isa().frontend_config();
                b.finalize(frontend_config);
            }
            self.module
                .define_function(cl_id, &mut ctx)
                .map_err(|e| SubsetError {
                    func: f.name.clone(),
                    message: format!("define failed: {e}"),
                })?;
        }
        self.module
            .finalize_definitions()
            .map_err(|e| SubsetError {
                func: String::from("<module>"),
                message: format!("finalize failed: {e}"),
            })?;
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.funcs.contains_key(name)
    }

    pub fn ptr(&self, name: &str) -> *const u8 {
        self.module.get_finalized_function(self.funcs[name])
    }

    /// Call a finalized gate function: `(ctx, args...) -> u64`.
    /// Supports up to 3 params (the gate corpus stays within that).
    ///
    /// # Safety
    /// Pointer must come from `ptr`, `ctx` must be the live `JitRt`,
    /// arity must match, and values crossing the boundary must be
    /// plain immediates or encoded handles.
    pub unsafe fn call_ptr(ptr: *const u8, ctx: *mut crate::runtime::JitRt, args: &[u64]) -> u64 {
        unsafe {
            match args {
                [] => {
                    let f: unsafe extern "C" fn(u64) -> u64 = std::mem::transmute(ptr);
                    f(ctx as u64)
                }
                [a0] => {
                    let f: unsafe extern "C" fn(u64, u64) -> u64 = std::mem::transmute(ptr);
                    f(ctx as u64, *a0)
                }
                [a0, a1] => {
                    let f: unsafe extern "C" fn(u64, u64, u64) -> u64 = std::mem::transmute(ptr);
                    f(ctx as u64, *a0, *a1)
                }
                [a0, a1, a2] => {
                    let f: unsafe extern "C" fn(u64, u64, u64, u64) -> u64 =
                        std::mem::transmute(ptr);
                    f(ctx as u64, *a0, *a1, *a2)
                }
                _ => panic!("gate calls support up to 3 args"),
            }
        }
    }
}

/// Per-function lowering state: MIR local → CLIF value, handle-typed
/// locals (for publication), and the shadow base.
struct FnCx {
    func: String,
    ctx: cranelift_codegen::ir::Value,
    values: HashMap<usize, cranelift_codegen::ir::Value>,
    /// Locals whose `Ty` is heap-managed (published at safepoints).
    handle_locals: HashMap<usize, ()>,
    /// Shadow depth at entry (epilogue pops to it).
    base: Option<cranelift_codegen::ir::Value>,
    /// `FuncRef` resolutions seen so far: MIR local → program fn index.
    funcrefs: HashMap<usize, usize>,
}

type CVal = cranelift_codegen::ir::Value;

impl FnCx {
    fn new(func: &str, params: Vec<CVal>) -> Self {
        let ctx = params[0];
        let mut values = HashMap::new();
        // Parameters are locals `0..params.len()-1` (ctx is not a MIR
        // local); all are defined from function entry.
        for (i, v) in params.iter().skip(1).enumerate() {
            values.insert(i, *v);
        }
        Self {
            func: func.to_string(),
            ctx,
            values,
            handle_locals: HashMap::new(),
            base: None,
            funcrefs: HashMap::new(),
        }
    }

    fn fail<T>(&self, message: String) -> Result<T, SubsetError> {
        Err(SubsetError {
            func: self.func.clone(),
            message,
        })
    }

    fn local_ty<'p>(
        &self,
        _prog: &'p MProgram,
        f: &'p toto_compiler::mir::MFn,
        local: usize,
    ) -> Result<&'p Ty, SubsetError> {
        f.locals
            .get(local)
            .map(|(_, ty)| ty)
            .ok_or_else(|| SubsetError {
                func: self.func.clone(),
                message: format!("local {local} out of range"),
            })
    }

    /// Trafficable types: immediates traffic as bit patterns, heap
    /// families as the handle codec. Returns true for handle types.
    /// Anything else is out of subset (the gate refuses rather than
    /// miscompiling a representation).
    fn trafficable(ty: &Ty) -> Result<bool, String> {
        match ty {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Unit => Ok(false),
            Ty::String | Ty::Array(_) | Ty::Struct(_) => Ok(true),
            other => Err(format!("type {other:?} is out of gate subset")),
        }
    }

    fn iconst(b: &mut FunctionBuilder<'_>, v: i64) -> CVal {
        b.ins().iconst(I64, v)
    }

    /// Branch to a `toto_panic` block when `cond` is nonzero; the
    /// panic block diverges, so control continues in a fresh block.
    /// No trap instruction: the unreachable path returns a dummy.
    fn emit_check(
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        ctx: CVal,
        cond: CVal,
        code: u64,
        span: (u64, u64),
        msg: &'static str,
    ) {
        use cranelift_codegen::ir::condcodes::IntCC::*;
        // Strict statement sequencing throughout this file: no nested
        // `b.ins()` calls (each borrows the builder mutably).
        let zero = Self::iconst(b, 0);
        let nonzero = b.ins().icmp(NotEqual, cond, zero);
        let panic_b = b.create_block();
        let cont_b = b.create_block();
        b.ins().brif(nonzero, panic_b, &[], cont_b, &[]);
        b.switch_to_block(panic_b);
        b.seal_block(panic_b);
        let callee = jit
            .module
            .declare_func_in_func(jit.imports["toto_panic"], b.func);
        let code_v = Self::iconst(b, code as i64);
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        let msg_ptr = Self::iconst(b, msg.as_ptr() as u64 as i64);
        let msg_len = Self::iconst(b, msg.len() as i64);
        b.ins()
            .call(callee, &[ctx, code_v, start_v, end_v, msg_ptr, msg_len]);
        // Unreachable (`toto_panic` diverges): return a dummy so the
        // block is terminated without any trap instruction.
        let dummy = Self::iconst(b, 0);
        let ret = [dummy];
        b.ins().return_(&ret);
        b.switch_to_block(cont_b);
        b.seal_block(cont_b);
    }

    /// Push every defined handle local (conservative publication).
    fn publish_all(
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        ctx: CVal,
        values: &HashMap<usize, CVal>,
        handle_locals: &HashMap<usize, ()>,
    ) {
        let mut ids: Vec<usize> = handle_locals.keys().copied().collect();
        ids.sort();
        for id in ids {
            if let Some(v) = values.get(&id) {
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_shadow_push"], b.func);
                b.ins().call(callee, &[ctx, *v]);
            }
        }
    }

    /// Lower one function body (single block + `Return` only).
    fn lower_fn(
        &mut self,
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
        f: &toto_compiler::mir::MFn,
    ) -> Result<(), SubsetError> {
        use toto_compiler::mir::MTerm;
        if f.blocks.len() != 1 {
            return self.fail(format!(
                "function has {} blocks (gate supports single-block bodies)",
                f.blocks.len()
            ));
        }
        let block = &f.blocks[0];
        if block.id != f.entry {
            return self.fail(String::from("entry is not the only block"));
        }
        // Return type must traffic.
        if Self::trafficable(&f.ret).is_err() {
            return self.fail(format!("return type {:?} is out of gate subset", f.ret));
        }
        // Mark handle-typed params (pre-defined locals).
        for (i, (_, ty)) in f.params.iter().enumerate() {
            match Self::trafficable(ty) {
                Ok(true) => {
                    self.handle_locals.insert(i, ());
                }
                Ok(false) => {}
                Err(e) => return self.fail(format!("param {i}: {e}")),
            }
        }
        // Prologue: record the shadow base (epilogue pops to it).
        let callee = jit
            .module
            .declare_func_in_func(jit.imports["toto_shadow_len"], b.func);
        let inst = b.ins().call(callee, &[self.ctx]);
        self.base = Some(b.inst_results(inst)[0]);
        for (i, instr) in block.instrs.iter().enumerate() {
            self.lower_instr(jit, b, prog, f, &block.instrs, i, instr)?;
        }
        match &block.term {
            MTerm::Return(Some(local)) => {
                let v = self.operand(*local)?;
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_shadow_pop_to"], b.func);
                let base = self.base.expect("prologue recorded base");
                b.ins().call(callee, &[self.ctx, base]);
                b.ins().return_(&[v]);
            }
            MTerm::Return(None) => {
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_shadow_pop_to"], b.func);
                let base = self.base.expect("prologue recorded base");
                b.ins().call(callee, &[self.ctx, base]);
                let z = Self::iconst(b, 0);
                let ret = [z];
                b.ins().return_(&ret);
            }
            other => {
                return self.fail(format!("terminator {other:?} is out of gate subset"));
            }
        }
        Ok(())
    }

    fn operand(&self, local: usize) -> Result<CVal, SubsetError> {
        self.values.get(&local).copied().ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("local {local} used before definition"),
        })
    }

    /// Structured branch returning a value (both arms join with a
    /// block param). `cond` is a raw comparison result, like Spike 3's
    /// `brif` use.
    fn if_else(
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        cond: CVal,
        then_branch: impl FnOnce(&mut Jit, &mut FunctionBuilder<'_>) -> CVal,
        else_branch: impl FnOnce(&mut Jit, &mut FunctionBuilder<'_>) -> CVal,
    ) -> CVal {
        let then_b = b.create_block();
        let else_b = b.create_block();
        let merge_b = b.create_block();
        b.append_block_param(merge_b, I64);
        b.ins().brif(cond, then_b, &[], else_b, &[]);
        b.switch_to_block(then_b);
        b.seal_block(then_b);
        let v1 = then_branch(jit, b);
        b.ins().jump(merge_b, &[BlockArg::Value(v1)]);
        b.switch_to_block(else_b);
        b.seal_block(else_b);
        let v0 = else_branch(jit, b);
        b.ins().jump(merge_b, &[BlockArg::Value(v0)]);
        b.switch_to_block(merge_b);
        b.seal_block(merge_b);
        b.block_params(merge_b)[0]
    }

    /// Integer arithmetic with the frozen explicit checks (`E0201`
    /// overflow, `E0202` zero divisor with the `DivCheck` span,
    /// `E0209` is handled at `Index`). Semantics mirror `ops`
    /// exactly, including `MIN/-1` division overflow and
    /// `wrapping_rem` for `%`.
    #[allow(clippy::too_many_arguments)]
    fn lower_bin(
        &mut self,
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        op: MBinOp,
        a: CVal,
        c: CVal,
        lty: &Ty,
        rty: &Ty,
        span: (u64, u64),
        divcheck_span: (u64, u64),
    ) -> Result<CVal, SubsetError> {
        use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
        let fail_op = |op: MBinOp, lty: &Ty, rty: &Ty| SubsetError {
            func: self.func.clone(),
            message: format!("{op:?} on {lty:?} and {rty:?} is out of gate subset"),
        };
        match (op, lty, rty) {
            (MBinOp::Add, Ty::Int, Ty::Int) => {
                let r = b.ins().iadd(a, c);
                // Overflow iff signs agree and the result disagrees.
                let zero = Self::iconst(b, 0);
                let t1 = b.ins().bxor(a, c);
                let c1 = b.ins().icmp(IntCC::SignedGreaterThanOrEqual, t1, zero);
                let same = b.ins().uextend(I64, c1);
                let t2 = b.ins().bxor(r, a);
                let c2 = b.ins().icmp(IntCC::SignedLessThan, t2, zero);
                let diff = b.ins().uextend(I64, c2);
                let ov = b.ins().band(same, diff);
                Self::emit_check(jit, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(r)
            }
            (MBinOp::Sub, Ty::Int, Ty::Int) => {
                let r = b.ins().isub(a, c);
                // Overflow iff signs disagree and the result disagrees
                // with the minuend.
                let zero = Self::iconst(b, 0);
                let t1 = b.ins().bxor(a, c);
                let c1 = b.ins().icmp(IntCC::SignedLessThan, t1, zero);
                let ds = b.ins().uextend(I64, c1);
                let t2 = b.ins().bxor(r, a);
                let c2 = b.ins().icmp(IntCC::SignedLessThan, t2, zero);
                let dr = b.ins().uextend(I64, c2);
                let ov = b.ins().band(ds, dr);
                Self::emit_check(jit, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(r)
            }
            (MBinOp::Mul, Ty::Int, Ty::Int) => {
                let r = b.ins().imul(a, c);
                let zero = Self::iconst(b, 0);
                let neg_one = Self::iconst(b, -1);
                let min = Self::iconst(b, i64::MIN);
                // Conditions have no side effects and never trap, so
                // all four are computed up front; only the division
                // below is path-guarded by the branch tree.
                let e_a0 = b.ins().icmp(IntCC::Equal, a, zero);
                let e_c0 = b.ins().icmp(IntCC::Equal, c, zero);
                let e_an1 = b.ins().icmp(IntCC::Equal, a, neg_one);
                let e_cn1 = b.ins().icmp(IntCC::Equal, c, neg_one);
                // Branch tree: zero never overflows; -1 overflows only
                // against MIN; otherwise `(a*b)/a == b` iff exact
                // (compiler-rt rule).
                let ov = Self::if_else(
                    jit,
                    b,
                    e_a0,
                    |_, b| Self::iconst(b, 0),
                    |jit, b| {
                        Self::if_else(
                            jit,
                            b,
                            e_c0,
                            |_, b| Self::iconst(b, 0),
                            |jit, b| {
                                Self::if_else(
                                    jit,
                                    b,
                                    e_an1,
                                    |_, b| {
                                        let e = b.ins().icmp(IntCC::Equal, c, min);
                                        b.ins().uextend(I64, e)
                                    },
                                    |jit, b| {
                                        Self::if_else(
                                            jit,
                                            b,
                                            e_cn1,
                                            |_, b| {
                                                let e = b.ins().icmp(IntCC::Equal, a, min);
                                                b.ins().uextend(I64, e)
                                            },
                                            |_, b| {
                                                let q = b.ins().sdiv(r, a);
                                                let ne = b.ins().icmp(IntCC::NotEqual, q, c);
                                                b.ins().uextend(I64, ne)
                                            },
                                        )
                                    },
                                )
                            },
                        )
                    },
                );
                Self::emit_check(jit, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(r)
            }
            (MBinOp::Div, Ty::Int, Ty::Int) => {
                let zero = Self::iconst(b, 0);
                let z = b.ins().icmp(IntCC::Equal, c, zero);
                let z64 = b.ins().uextend(I64, z);
                Self::emit_check(
                    jit,
                    b,
                    self.ctx,
                    z64,
                    E_DIV_ZERO,
                    divcheck_span,
                    MSG_DIV_ZERO,
                );
                // `MIN/-1` is the only overflow (`checked_div` parity).
                let min = Self::iconst(b, i64::MIN);
                let neg_one = Self::iconst(b, -1);
                let e1 = b.ins().icmp(IntCC::Equal, a, min);
                let a_min = b.ins().uextend(I64, e1);
                let e2 = b.ins().icmp(IntCC::Equal, c, neg_one);
                let b_neg = b.ins().uextend(I64, e2);
                let ov = b.ins().band(a_min, b_neg);
                Self::emit_check(jit, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(b.ins().sdiv(a, c))
            }
            (MBinOp::Mod, Ty::Int, Ty::Int) => {
                let zero = Self::iconst(b, 0);
                let z = b.ins().icmp(IntCC::Equal, c, zero);
                let z64 = b.ins().uextend(I64, z);
                Self::emit_check(
                    jit,
                    b,
                    self.ctx,
                    z64,
                    E_DIV_ZERO,
                    divcheck_span,
                    MSG_DIV_ZERO,
                );
                // `MIN % -1 == 0` (`wrapping_rem` parity); `srem`
                // would fault on that input, so branch around it.
                let min = Self::iconst(b, i64::MIN);
                let neg_one = Self::iconst(b, -1);
                let e1 = b.ins().icmp(IntCC::Equal, a, min);
                let e2 = b.ins().icmp(IntCC::Equal, c, neg_one);
                let special = b.ins().band(e1, e2);
                Ok(Self::if_else(
                    jit,
                    b,
                    special,
                    |_, b| Self::iconst(b, 0),
                    |_, b| b.ins().srem(a, c),
                ))
            }
            (MBinOp::Add, Ty::Float, Ty::Float) => Ok(b.ins().fadd(a, c)),
            (MBinOp::Sub, Ty::Float, Ty::Float) => Ok(b.ins().fsub(a, c)),
            (MBinOp::Mul, Ty::Float, Ty::Float) => Ok(b.ins().fmul(a, c)),
            (MBinOp::Div, Ty::Float, Ty::Float) => Ok(b.ins().fdiv(a, c)),
            (MBinOp::Eq, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::Equal, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::NotEq, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::NotEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Lt, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::SignedLessThan, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::LtEq, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::SignedLessThanOrEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Gt, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::SignedGreaterThan, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::GtEq, Ty::Int, Ty::Int) => {
                let e = b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Eq, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::Equal, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::NotEq, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::NotEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Lt, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::LessThan, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::LtEq, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::LessThanOrEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Gt, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::GreaterThan, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::GtEq, Ty::Float, Ty::Float) => {
                let e = b.ins().fcmp(FloatCC::GreaterThanOrEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            _ => Err(fail_op(op, lty, rty)),
        }
    }

    /// Value bits plus transport tag for `toto_write_field`, by MIR type.
    fn tag_of(ty: &Ty) -> Result<u64, String> {
        match ty {
            Ty::Int => Ok(TAG_INT),
            Ty::Float => Ok(TAG_FLOAT),
            Ty::Bool => Ok(TAG_BOOL),
            Ty::String | Ty::Array(_) | Ty::Struct(_) => Ok(TAG_HANDLE),
            Ty::Unit => Ok(TAG_UNIT),
            other => Err(format!("type {other:?} cannot cross the JIT boundary")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_instr(
        &mut self,
        jit: &mut Jit,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
        f: &toto_compiler::mir::MFn,
        instrs: &[MInstr],
        idx: usize,
        instr: &MInstr,
    ) -> Result<(), SubsetError> {
        match instr {
            MInstr::Const { dst, val } => {
                let v = match val {
                    MConst::Int(i) => Self::iconst(b, *i),
                    MConst::Bool(bv) => Self::iconst(b, i64::from(*bv)),
                    MConst::Unit | MConst::None => Self::iconst(b, 0),
                    MConst::Float(f) => {
                        b.ins()
                            .f64const(cranelift_codegen::ir::immediates::Ieee64::with_bits(
                                f.to_bits(),
                            ))
                    }
                    MConst::Str(s) => {
                        // Gate-owned bytes (leaked for the JIT's
                        // lifetime; Phase 4 uses rodata). Publishing
                        // first: other live handles must survive the
                        // threshold check inside the allocator.
                        Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                        let leaked: &'static str = Box::leak(s.clone().into_boxed_str());
                        let ptr = Self::iconst(b, leaked.as_ptr() as u64 as i64);
                        let len = Self::iconst(b, leaked.len() as i64);
                        // Span of a const: no meaningful span exists;
                        // allocation itself cannot fail on valid UTF-8
                        // (leaked from a checked literal), so zeros.
                        let callee = jit
                            .module
                            .declare_func_in_func(jit.imports["toto_alloc_str"], b.func);
                        let z0 = Self::iconst(b, 0);
                        let z1 = Self::iconst(b, 0);
                        let args = [self.ctx, ptr, len, z0, z1];
                        let inst = b.ins().call(callee, &args);
                        b.inst_results(inst)[0]
                    }
                };
                let ty = self.local_ty(prog, f, *dst)?;
                if Self::trafficable(ty).is_err() {
                    return self.fail(format!("const target type {ty:?} is out of gate subset"));
                }
                if Self::trafficable(ty).expect("checked above") {
                    self.handle_locals.insert(*dst, ());
                }
                self.values.insert(*dst, v);
            }
            MInstr::Copy { dst, src } => {
                let v = self.operand(*src)?;
                let ty = self.local_ty(prog, f, *dst)?;
                if Self::trafficable(ty).is_err() {
                    return self.fail(format!("copy target type {ty:?} is out of gate subset"));
                }
                if Self::trafficable(ty).expect("checked above") {
                    self.handle_locals.insert(*dst, ());
                }
                self.values.insert(*dst, v);
            }
            MInstr::Bin {
                dst,
                op,
                l,
                r,
                span,
            } => {
                let lty = self.local_ty(prog, f, *l)?.clone();
                let rty = self.local_ty(prog, f, *r)?.clone();
                // `Div`/`Mod` must be paired with an immediately
                // preceding `DivCheck` on the divisor (Tier-1 pins
                // this structurally; the gate verifies instead of
                // trusting, and reports the check's span on failure —
                // exactly like the interpreter, where `DivCheck` fires
                // first).
                let divcheck_span = if matches!(op, MBinOp::Div | MBinOp::Mod) {
                    match (idx.checked_sub(1), instrs.get(idx.wrapping_sub(1))) {
                        (Some(_), Some(MInstr::DivCheck { value, span: dspan }))
                            if *value == *r =>
                        {
                            (dspan.start as u64, dspan.end as u64)
                        }
                        _ => {
                            return self.fail(format!(
                                "{op:?} without an immediately-preceding DivCheck on local {r}"
                            ));
                        }
                    }
                } else {
                    (0, 0)
                };
                let a = self.operand(*l)?;
                let c = self.operand(*r)?;
                let v = self.lower_bin(
                    jit,
                    b,
                    *op,
                    a,
                    c,
                    &lty,
                    &rty,
                    (span.start as u64, span.end as u64),
                    divcheck_span,
                )?;
                let dty = self.local_ty(prog, f, *dst)?;
                if Self::trafficable(dty).is_err() {
                    return self.fail(format!("bin target type {dty:?} is out of gate subset"));
                }
                self.values.insert(*dst, v);
            }
            MInstr::Neg { dst, operand, span } => {
                let v = self.operand(*operand)?;
                let ty = self.local_ty(prog, f, *operand)?.clone();
                let r = match ty {
                    Ty::Int => {
                        let min = Self::iconst(b, i64::MIN);
                        let is_min =
                            b.ins()
                                .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, v, min);
                        let is_min64 = b.ins().uextend(I64, is_min);
                        Self::emit_check(
                            jit,
                            b,
                            self.ctx,
                            is_min64,
                            E_OVERFLOW,
                            (span.start as u64, span.end as u64),
                            MSG_OVERFLOW,
                        );
                        let zero = Self::iconst(b, 0);
                        b.ins().isub(zero, v)
                    }
                    Ty::Float => b.ins().fneg(v),
                    other => {
                        return self.fail(format!("neg on {other:?} is out of gate subset"));
                    }
                };
                self.values.insert(*dst, r);
            }
            MInstr::Not { dst, operand, .. } => {
                let v = self.operand(*operand)?;
                let ty = self.local_ty(prog, f, *operand)?.clone();
                match ty {
                    Ty::Bool => {
                        let one = Self::iconst(b, 1);
                        self.values.insert(*dst, b.ins().bxor(v, one));
                    }
                    other => {
                        return self.fail(format!("not on {other:?} is out of gate subset"));
                    }
                }
            }
            MInstr::DivCheck { .. } => {
                // Verified at each `Div`/`Mod` (the check must
                // immediately precede with the same divisor); the
                // codegen emits its own span-faithful check there.
            }
            MInstr::Call {
                dst,
                func,
                args,
                span,
                display,
            } => {
                let _ = (span, display);
                let target = self.funcrefs.get(func).copied().ok_or_else(|| SubsetError {
                    func: self.func.clone(),
                    message: format!(
                        "indirect call through local {func} is out of gate subset (only FuncRef-resolved static calls)"
                    ),
                })?;
                let callee_fn = prog.fns.get(target).ok_or_else(|| SubsetError {
                    func: self.func.clone(),
                    message: format!("funcref index {target} out of range"),
                })?;
                if callee_fn.params.len() != args.len() {
                    return self.fail(format!(
                        "call arity {} vs {} params of {}",
                        args.len(),
                        callee_fn.params.len(),
                        callee_fn.name
                    ));
                }
                let cl_id = jit
                    .funcs
                    .get(&callee_fn.name)
                    .copied()
                    .ok_or_else(|| SubsetError {
                        func: self.func.clone(),
                        message: format!("callee {} not compiled", callee_fn.name),
                    })?;
                // Publish before the call: the callee may allocate and
                // collect (frozen safepoint-call rule).
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                let mut vals = vec![self.ctx];
                for a in args {
                    let ty = self.local_ty(prog, f, *a)?.clone();
                    if Self::trafficable(&ty).is_err() {
                        return self.fail(format!("call arg type {ty:?} is out of gate subset"));
                    }
                    vals.push(self.operand(*a)?);
                }
                let callee = jit.module.declare_func_in_func(cl_id, b.func);
                let inst = b.ins().call(callee, &vals);
                let ret = b.inst_results(inst)[0];
                if let Some(d) = dst {
                    let dty = self.local_ty(prog, f, *d)?;
                    match Self::trafficable(dty) {
                        Ok(true) => {
                            self.handle_locals.insert(*d, ());
                        }
                        Ok(false) => {}
                        Err(e) => return self.fail(format!("call target: {e}")),
                    }
                    self.values.insert(*d, ret);
                }
            }
            MInstr::FuncRef { dst, idx } => {
                if prog.fns.get(*idx).is_none() {
                    return self.fail(format!("funcref index {idx} out of range"));
                }
                // No code: resolved statically at each `Call`.
                self.funcrefs.insert(*dst, *idx);
            }
            MInstr::CallBuiltin {
                dst,
                name,
                args,
                span,
            } => {
                if name != "len" || args.len() != 1 {
                    return self.fail(format!(
                        "builtin `{name}` is out of gate subset (only single-arg `len` on arrays)"
                    ));
                }
                let base = self.operand(args[0])?;
                let bty = self.local_ty(prog, f, args[0])?.clone();
                if !matches!(bty, Ty::Array(_)) {
                    return self.fail(format!("len on {bty:?} is out of gate subset"));
                }
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_array_len"], b.func);
                let start = Self::iconst(b, span.start as i64);
                let end = Self::iconst(b, span.end as i64);
                let args = [self.ctx, base, start, end];
                let inst = b.ins().call(callee, &args);
                let v = b.inst_results(inst)[0];
                if let Some(d) = dst {
                    self.values.insert(*d, v);
                }
            }
            MInstr::ArrayLit { dst, elems, span } => {
                // Publish first (threshold check inside the allocator),
                // then allocate plus one initializing write per element.
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_alloc_array"], b.func);
                let alen = Self::iconst(b, elems.len() as i64);
                let astart = Self::iconst(b, span.start as i64);
                let aend = Self::iconst(b, span.end as i64);
                let aargs = [self.ctx, alen, astart, aend];
                let inst = b.ins().call(callee, &aargs);
                let arr = b.inst_results(inst)[0];
                for (i, e) in elems.iter().enumerate() {
                    let v = self.operand(*e)?;
                    let ety = self.local_ty(prog, f, *e)?.clone();
                    let tag = Self::tag_of(&ety).map_err(|e| SubsetError {
                        func: self.func.clone(),
                        message: format!("array element: {e}"),
                    })?;
                    let wcallee = jit
                        .module
                        .declare_func_in_func(jit.imports["toto_write_field"], b.func);
                    let fi = Self::iconst(b, i as i64);
                    let ftag = Self::iconst(b, tag as i64);
                    let fstart = Self::iconst(b, span.start as i64);
                    let fend = Self::iconst(b, span.end as i64);
                    let fargs = [self.ctx, arr, fi, ftag, v, fstart, fend];
                    b.ins().call(wcallee, &fargs);
                }
                let dty = self.local_ty(prog, f, *dst)?;
                if !matches!(dty, Ty::Array(_)) {
                    return self.fail(format!("array target type {dty:?} is out of gate subset"));
                }
                self.handle_locals.insert(*dst, ());
                self.values.insert(*dst, arr);
            }
            MInstr::Index {
                dst,
                base,
                index,
                span,
                ..
            } => {
                let bty = self.local_ty(prog, f, *base)?.clone();
                let ity = self.local_ty(prog, f, *index)?.clone();
                if !matches!(bty, Ty::Array(_)) || !matches!(ity, Ty::Int) {
                    return self.fail(format!(
                        "index of {bty:?} by {ity:?} is out of gate subset (array by int only)"
                    ));
                }
                // No publication needed before a read (no collection
                // inside), but the base must be live: it is defined,
                // and every safepoint since its definition published it.
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_read_field"], b.func);
                let bbase = self.operand(*base)?;
                let bidx = self.operand(*index)?;
                let bstart = Self::iconst(b, span.start as i64);
                let bend = Self::iconst(b, span.end as i64);
                let bargs = [self.ctx, bbase, bidx, bstart, bend];
                let inst = b.ins().call(callee, &bargs);
                let v = b.inst_results(inst)[0];
                let dty = self.local_ty(prog, f, *dst)?;
                if Self::trafficable(dty).is_err() {
                    return self.fail(format!("index target type {dty:?} is out of gate subset"));
                }
                if Self::trafficable(dty).expect("checked above") {
                    self.handle_locals.insert(*dst, ());
                }
                self.values.insert(*dst, v);
            }
            MInstr::Len { dst, base, span } => {
                let bty = self.local_ty(prog, f, *base)?.clone();
                if !matches!(bty, Ty::Array(_)) {
                    return self.fail(format!("len on {bty:?} is out of gate subset"));
                }
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
                let callee = jit
                    .module
                    .declare_func_in_func(jit.imports["toto_array_len"], b.func);
                let lbase = self.operand(*base)?;
                let lstart = Self::iconst(b, span.start as i64);
                let lend = Self::iconst(b, span.end as i64);
                let largs = [self.ctx, lbase, lstart, lend];
                let inst = b.ins().call(callee, &largs);
                self.values.insert(*dst, b.inst_results(inst)[0]);
            }
            MInstr::Safepoint { kind } => {
                use toto_compiler::mir::SpKind as K;
                match kind {
                    K::Entry => {
                        // Base already recorded in the prologue; the
                        // publish below covers handle params.
                    }
                    K::Call | K::Alloc | K::Loop | K::Park => {}
                }
                Self::publish_all(jit, b, self.ctx, &self.values, &self.handle_locals);
            }
            other => {
                return self.fail(format!("{other:?} is out of gate subset"));
            }
        }
        Ok(())
    }
}
