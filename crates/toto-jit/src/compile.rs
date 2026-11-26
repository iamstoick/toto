//! MIR-subset → Cranelift backend (Phase 4, Stage 1).
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
use std::sync::Arc;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, Endianness, FuncRef, Function, InstBuilder, MemFlagsData, Signature,
    StackSlotData, StackSlotKind, types::*,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClFuncId, Linkage, Module};
use toto_compiler::hir::Ty;
use toto_compiler::mir::{MBinOp, MBlock, MConst, MInstr, MProgram, MSelectCase, MTerm, MirBody};
use toto_compiler::span::Span;

use crate::runtime::{
    E_DIV_ZERO, E_FALLOFF, E_INTERNAL, E_MISMATCH, E_OVERFLOW, TAG_BOOL, TAG_CHAN, TAG_FLOAT,
    TAG_FN_ANY, TAG_HANDLE, TAG_INT, TAG_PTR, TAG_UNIT,
};

/// Gate boundary: MIR outside the supported subset.
#[derive(Clone, Debug)]
pub struct SubsetError {
    pub func: String,
    pub message: String,
}

impl fmt::Display for SubsetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "toto-jit refuse: {}: {}", self.func, self.message)
    }
}

impl std::error::Error for SubsetError {}

/// Panic message bytes owned by the gate (addresses baked into
/// generated code as immediates; gate-only — Phase 4 uses rodata).
static MSG_DIV_ZERO: &str = "division by zero";
static MSG_OVERFLOW: &str = "integer overflow";

pub(crate) fn sig_arity(params: u32, returns: bool) -> Signature {
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

/// Codegen mode: diverging panics on a standalone context (gate
/// harness) vs recording panics with suspension on scheduler tasks
/// (production path). One mode per `Jit` session: import names are
/// shared where the contract matches, but the implementations (and
/// some signatures) differ by context type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodegenMode {
    Gate,
    Task,
}

/// Cranelift optimization level for generated code (Phase 4, Stage 3:
/// the minimal flagged pipeline — one flag, three levels, no passes
/// of our own). `Off` is the default and preserves the exact
/// Stage-1 gate behavior (`opt_level=none`); `Speed` and
/// `SpeedAndSize` select Cranelift's corresponding pipelines. The
/// level affects machine-code emission only: MIR, the value codec,
/// entry points, and root publication are identical at every level
/// (pinned by `opt.rs` equivalence).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    /// `opt_level=none`: fastest compile, Stage-1 behavior.
    #[default]
    Off,
    /// `opt_level=speed`.
    Speed,
    /// `opt_level=speed_and_size`.
    SpeedAndSize,
}

impl OptLevel {
    /// The Cranelift `optlevel`/`opt_level` flag value.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            OptLevel::Off => "none",
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
        }
    }

    /// Parse a CLI/test flag value (`off`, `speed`, `speed_and_size`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(OptLevel::Off),
            "speed" => Some(OptLevel::Speed),
            "speed_and_size" => Some(OptLevel::SpeedAndSize),
            _ => None,
        }
    }
}

/// What a lowered body is: top-level functions take `(ctx, args)`,
/// closures take `(ctx, handle, args)`, go bodies take `(task_ctx)`.
/// Only `Closure`/`Go` are ever collected (top-level functions are
/// lowered directly, never through the body table).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyKind {
    TopLevel,
    Closure,
    Go,
}

/// Borrowed body shape shared by top-level functions and nested
/// bodies: the lowering core is identical; only the prologue differs
/// (plain params vs handle/capture fetches).
pub(crate) struct BodyShape<'a> {
    pub(crate) name: &'a str,
    pub(crate) params: &'a [(String, Ty)],
    pub(crate) ret: &'a Ty,
    pub(crate) locals: &'a [(String, Ty)],
    pub(crate) blocks: &'a [MBlock],
    pub(crate) entry: usize,
    pub(crate) body_span: Span,
    pub(crate) kind: BodyKind,
    /// Capture slots appended after the params (nested bodies only).
    pub(crate) ncaptures: usize,
}

/// Hidden body function name (the `$` suffix is unspellable in Toto
/// source, so user functions can never collide with it).
pub(crate) fn body_fn_name(id: u32) -> String {
    format!("toto_body{id}$")
}

/// Codegen session abstraction shared by the in-memory JIT (`Jit`)
/// and the AOT object driver (`ObjSession` in `aot.rs`): everything
/// the lowering core (`FnCx`) needs from its session. Import keys are
/// always the `toto_*` contract names; each session maps them to its
/// own link symbols (JIT registers host addresses, AOT declares
/// `task_*` imports resolved at link time).
pub(crate) trait Session {
    /// Gate or task import set / codegen flavor (AOT is task-mode).
    fn mode(&self) -> CodegenMode;
    /// Declare (or fetch) a runtime import; returns its function id.
    fn import(&mut self, name: &'static str, params: u32, returns: bool) -> ClFuncId;
    /// Look up a declared import.
    fn import_id(&self, name: &'static str) -> Option<ClFuncId>;
    /// Emit `(ptr, len)` values for static bytes `s` into `b`
    /// (JIT: session addresses; AOT: module-data references).
    fn intern_str(&mut self, b: &mut FunctionBuilder, s: &str) -> (CVal, CVal);
    /// Reference a declared function inside `func`.
    fn declare_in_func(&mut self, id: ClFuncId, func: &mut Function) -> FuncRef;
    /// Look up a declared user function or body by name.
    fn func_id(&self, name: &str) -> Option<ClFuncId>;
    /// Collect a nested body; returns its body id.
    fn alloc_body(&mut self, kind: BodyKind, body: Arc<MirBody>) -> u32;
    /// Intern a channel element type; returns its type id.
    fn intern_chan_type(&mut self, elem: Ty) -> u64;
}

/// Shared runtime-import declaration list (JIT and AOT object
/// drivers): keys are the `toto_*` contract names; each session maps
/// them to its own link symbols. `task` selects the task-mode set
/// (AOT is always task-mode: it supports concurrent programs).
pub(crate) fn declare_runtime_imports(sess: &mut impl Session, task: bool) {
    // Arity is words after ctx; `returns` matches the registered
    // implementation (gate `toto_panic` diverges, task records).
    sess.import("toto_alloc_array", 3, true);
    sess.import("toto_alloc_str", 4, true);
    sess.import("toto_alloc_struct", 4, true);
    sess.import("toto_alloc_range", 2, true);
    sess.import("toto_alloc_result", 5, true);
    sess.import("toto_alloc_some", 4, true);
    sess.import("toto_alloc_closure", 5, true);
    sess.import("toto_index_read", 8, true);
    sess.import("toto_index_write", 10, true);
    sess.import("toto_struct_init", 7, true);
    sess.import("toto_struct_get", 5, true);
    sess.import("toto_struct_set", 7, true);
    sess.import("toto_str_concat", 4, true);
    sess.import("toto_range_lo", 3, true);
    sess.import("toto_range_hi", 3, true);
    sess.import("toto_result_is_ok", 3, true);
    sess.import("toto_wrap_inner", 3, true);
    sess.import("toto_closure_body", 3, true);
    sess.import("toto_closure_cap", 4, true);
    sess.import("toto_values_eq", 7, true);
    sess.import("toto_values_order", 7, true);
    sess.import("toto_builtin", 5, true);
    sess.import("toto_foreign", 5, true);
    sess.import("toto_len", 4, true);
    sess.import("toto_func_entry", 3, true);
    sess.import("toto_body_entry", 3, true);
    sess.import("toto_shadow_push", 1, false);
    sess.import("toto_shadow_len", 0, true);
    sess.import("toto_state_get", 3, true);
    sess.import("toto_state_set", 5, true);
    sess.import("toto_shadow_pop_to", 1, false);
    sess.import("toto_gc_collect", 0, true);
    sess.import("toto_panic", 5, task);
    if task {
        sess.import("toto_yield", 8, true);
        sess.import("toto_spawn", 5, true);
        sess.import("toto_chan_make", 5, true);
        sess.import("toto_check_failed", 4, true);
        sess.import("toto_task_capture", 3, true);
    }
}

pub struct Jit {
    module: JITModule,
    imports: HashMap<&'static str, ClFuncId>,
    funcs: HashMap<String, ClFuncId>,
    mode: CodegenMode,
    /// Owned static bytes (string literals, struct/field/function
    /// names, messages): addresses baked into generated code as
    /// immediates, freed with the session (no leak, no rodata yet).
    statics: Vec<Box<str>>,
    /// Collected nested bodies: assigned ids during lowering,
    /// compiled after their parents (fixpoint: bodies nest).
    bodies: Vec<(u32, BodyKind, Arc<MirBody>)>,
    next_body: u32,
    /// Interned channel element types (append-only; ids index it).
    chan_types: Vec<Ty>,
    /// Per-function codegen times in definition order
    /// (`(name, microseconds)`), for the compile-time report.
    pub timings: Vec<(String, u64)>,
}

impl Jit {
    pub fn new() -> Result<Self, String> {
        Self::with_mode_and_opt(CodegenMode::Gate, OptLevel::Off)
    }

    /// Task-mode session (production path): same import names where
    /// the contract matches, recording panics, suspension support.
    pub fn new_task() -> Result<Self, String> {
        Self::with_mode_and_opt(CodegenMode::Task, OptLevel::Off)
    }

    /// Gate-mode session at an explicit optimization level.
    pub fn new_with_opt(level: OptLevel) -> Result<Self, String> {
        Self::with_mode_and_opt(CodegenMode::Gate, level)
    }

    /// Task-mode session at an explicit optimization level.
    pub fn new_task_with_opt(level: OptLevel) -> Result<Self, String> {
        Self::with_mode_and_opt(CodegenMode::Task, level)
    }

    fn with_mode_and_opt(mode: CodegenMode, opt: OptLevel) -> Result<Self, String> {
        let mut jb = JITBuilder::with_flags(
            &[("opt_level", opt.flag())],
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| e.to_string())?;
        // One symbol table per mode (each takes ctx first). Import
        // names are shared where the contract matches; the Rust
        // implementations differ by context type.
        match mode {
            CodegenMode::Gate => {
                use crate::runtime as rt;
                jb.symbol("toto_alloc_array", rt::toto_alloc_array as *const u8);
                jb.symbol("toto_alloc_str", rt::toto_alloc_str as *const u8);
                jb.symbol("toto_alloc_struct", rt::toto_alloc_struct as *const u8);
                jb.symbol("toto_alloc_range", rt::toto_alloc_range as *const u8);
                jb.symbol("toto_alloc_result", rt::toto_alloc_result as *const u8);
                jb.symbol("toto_alloc_some", rt::toto_alloc_some as *const u8);
                jb.symbol("toto_alloc_closure", rt::toto_alloc_closure as *const u8);
                jb.symbol("toto_index_read", rt::toto_index_read as *const u8);
                jb.symbol("toto_index_write", rt::toto_index_write as *const u8);
                jb.symbol("toto_struct_init", rt::toto_struct_init as *const u8);
                jb.symbol("toto_struct_get", rt::toto_struct_get as *const u8);
                jb.symbol("toto_struct_set", rt::toto_struct_set as *const u8);
                jb.symbol("toto_str_concat", rt::toto_str_concat as *const u8);
                jb.symbol("toto_range_lo", rt::toto_range_lo as *const u8);
                jb.symbol("toto_range_hi", rt::toto_range_hi as *const u8);
                jb.symbol("toto_result_is_ok", rt::toto_result_is_ok as *const u8);
                jb.symbol("toto_wrap_inner", rt::toto_wrap_inner as *const u8);
                jb.symbol("toto_closure_body", rt::toto_closure_body as *const u8);
                jb.symbol("toto_closure_cap", rt::toto_closure_cap as *const u8);
                jb.symbol("toto_values_eq", rt::toto_values_eq as *const u8);
                jb.symbol("toto_values_order", rt::toto_values_order as *const u8);
                jb.symbol("toto_builtin", rt::toto_builtin as *const u8);
                jb.symbol("toto_foreign", rt::toto_foreign as *const u8);
                jb.symbol("toto_len", rt::toto_len as *const u8);
                jb.symbol("toto_func_entry", rt::toto_func_entry as *const u8);
                jb.symbol("toto_body_entry", rt::toto_body_entry as *const u8);
                jb.symbol("toto_shadow_push", rt::toto_shadow_push as *const u8);
                jb.symbol("toto_shadow_len", rt::toto_shadow_len as *const u8);
                jb.symbol("toto_shadow_pop_to", rt::toto_shadow_pop_to as *const u8);
                jb.symbol("toto_gc_collect", rt::toto_gc_collect as *const u8);
                jb.symbol("toto_state_get", rt::toto_state_get as *const u8);
                jb.symbol("toto_state_set", rt::toto_state_set as *const u8);
                jb.symbol("toto_panic", rt::toto_panic as *const u8);
            }
            CodegenMode::Task => {
                use crate::task as rt;
                jb.symbol("toto_alloc_array", rt::task_alloc_array as *const u8);
                jb.symbol("toto_alloc_str", rt::task_alloc_str as *const u8);
                jb.symbol("toto_alloc_struct", rt::task_alloc_struct as *const u8);
                jb.symbol("toto_alloc_range", rt::task_alloc_range as *const u8);
                jb.symbol("toto_alloc_result", rt::task_alloc_result as *const u8);
                jb.symbol("toto_alloc_some", rt::task_alloc_some as *const u8);
                jb.symbol("toto_alloc_closure", rt::task_alloc_closure as *const u8);
                jb.symbol("toto_index_read", rt::task_index_read as *const u8);
                jb.symbol("toto_index_write", rt::task_index_write as *const u8);
                jb.symbol("toto_struct_init", rt::task_struct_init as *const u8);
                jb.symbol("toto_struct_get", rt::task_struct_get as *const u8);
                jb.symbol("toto_struct_set", rt::task_struct_set as *const u8);
                jb.symbol("toto_str_concat", rt::task_str_concat as *const u8);
                jb.symbol("toto_range_lo", rt::task_range_lo as *const u8);
                jb.symbol("toto_range_hi", rt::task_range_hi as *const u8);
                jb.symbol("toto_result_is_ok", rt::task_result_is_ok as *const u8);
                jb.symbol("toto_wrap_inner", rt::task_wrap_inner as *const u8);
                jb.symbol("toto_closure_body", rt::task_closure_body as *const u8);
                jb.symbol("toto_closure_cap", rt::task_closure_cap as *const u8);
                jb.symbol("toto_values_eq", rt::task_values_eq as *const u8);
                jb.symbol("toto_values_order", rt::task_values_order as *const u8);
                jb.symbol("toto_builtin", rt::task_builtin as *const u8);
                jb.symbol("toto_foreign", rt::task_foreign as *const u8);
                jb.symbol("toto_len", rt::task_len as *const u8);
                jb.symbol("toto_func_entry", rt::task_func_entry as *const u8);
                jb.symbol("toto_body_entry", rt::task_body_entry as *const u8);
                jb.symbol("toto_shadow_push", rt::task_shadow_push as *const u8);
                jb.symbol("toto_shadow_len", rt::task_shadow_len as *const u8);
                jb.symbol("toto_shadow_pop_to", rt::task_shadow_pop_to as *const u8);
                jb.symbol("toto_gc_collect", rt::task_gc_collect as *const u8);
                jb.symbol("toto_state_get", rt::task_state_get as *const u8);
                jb.symbol("toto_state_set", rt::task_state_set as *const u8);
                jb.symbol("toto_panic", rt::task_panic as *const u8);
                jb.symbol("toto_yield", rt::task_yield as *const u8);
                jb.symbol("toto_spawn", rt::task_spawn as *const u8);
                jb.symbol("toto_chan_make", rt::task_chan_make as *const u8);
                jb.symbol("toto_check_failed", rt::task_check_failed as *const u8);
                jb.symbol("toto_task_capture", rt::task_capture as *const u8);
            }
        }
        let module = JITModule::new(jb);
        Ok(Self {
            module,
            imports: HashMap::new(),
            funcs: HashMap::new(),
            mode,
            statics: Vec::new(),
            bodies: Vec::new(),
            next_body: 0,
            chan_types: Vec::new(),
            timings: Vec::new(),
        })
    }

    /// Intern owned static bytes, returning the baked address.
    fn intern(&mut self, s: &str) -> (u64, u64) {
        let boxed: Box<str> = s.into();
        let ptr = boxed.as_ptr() as u64;
        let len = boxed.len() as u64;
        self.statics.push(boxed);
        (ptr, len)
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
        declare_runtime_imports(self, self.mode == CodegenMode::Task);
    }

    /// Compile every function in a real MIR program (gate mode).
    pub fn compile(&mut self, prog: &MProgram) -> Result<(), SubsetError> {
        if self.mode != CodegenMode::Gate {
            return Err(SubsetError {
                func: String::from("<module>"),
                message: String::from("compile() needs a gate session (Jit::new)"),
            });
        }
        self.compile_program(prog)
    }

    /// Compile every function in a real MIR program (task mode),
    /// returning the shared tables the runner and spawns need.
    pub fn compile_task(
        &mut self,
        prog: &MProgram,
    ) -> Result<crate::task::JitSession, SubsetError> {
        if self.mode != CodegenMode::Task {
            return Err(SubsetError {
                func: String::from("<module>"),
                message: String::from("compile_task() needs a task session (Jit::new_task)"),
            });
        }
        self.compile_program(prog)?;
        let mut entries = HashMap::new();
        for f in &prog.fns {
            entries.insert(f.name.clone(), self.ptr(&f.name) as usize);
        }
        let mut bodies = HashMap::new();
        for (id, _, _) in &self.bodies {
            let name = body_fn_name(*id);
            bodies.insert(*id, self.ptr(&name) as usize);
        }
        let func_names = prog.fns.iter().map(|f| f.name.clone()).collect();
        Ok(crate::task::JitSession {
            entries: Arc::new(entries),
            bodies: Arc::new(bodies),
            func_names: Arc::new(func_names),
            chan_types: Arc::new(std::mem::take(&mut self.chan_types)),
            statics: Arc::new(std::mem::take(&mut self.statics)),
        })
    }

    /// Shared driver: declare tops, lower tops (collecting nested
    /// bodies), compile bodies to fixpoint (bodies nest), finalize.
    /// Records per-function codegen times. A failed compile leaves
    /// the session dirty; start a new one (same as the gate).
    fn compile_program(&mut self, prog: &MProgram) -> Result<(), SubsetError> {
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
        for fi in 0..prog.fns.len() {
            self.define_top(prog, fi)?;
        }
        let mut done = 0usize;
        while done < self.bodies.len() {
            let pending: Vec<(u32, BodyKind, Arc<MirBody>)> = self.bodies[done..].to_vec();
            for (id, kind, body) in &pending {
                let nparams = match kind {
                    BodyKind::Closure => body.params.len() + 1,
                    BodyKind::Go => 0,
                    BodyKind::TopLevel => {
                        return Err(SubsetError {
                            func: body_fn_name(*id),
                            message: String::from("top-level body in the body table (unreachable)"),
                        });
                    }
                };
                let name = body_fn_name(*id);
                let sig = sig_arity(nparams as u32, true);
                let cl_id = self
                    .module
                    .declare_function(&name, Linkage::Export, &sig)
                    .map_err(|e| SubsetError {
                        func: name.clone(),
                        message: format!("declare failed: {e}"),
                    })?;
                self.funcs.insert(name, cl_id);
            }
            for (id, kind, body) in &pending {
                self.define_body(prog, *id, *kind, body)?;
            }
            done += pending.len();
        }
        self.module
            .finalize_definitions()
            .map_err(|e| SubsetError {
                func: String::from("<module>"),
                message: format!("finalize failed: {e}"),
            })?;
        Ok(())
    }

    fn define_top(&mut self, prog: &MProgram, fi: usize) -> Result<(), SubsetError> {
        let f = &prog.fns[fi];
        let cl_id = self.funcs[&f.name];
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
        self.define_common(prog, &f.name, cl_id, shape, &mut ctx, &mut fctx)
    }

    fn define_body(
        &mut self,
        prog: &MProgram,
        id: u32,
        kind: BodyKind,
        body: &MirBody,
    ) -> Result<(), SubsetError> {
        if self.mode == CodegenMode::Gate && kind == BodyKind::Go {
            return Err(SubsetError {
                func: body_fn_name(id),
                message: String::from("go bodies need a task session (concurrency is task-only)"),
            });
        }
        let name = body_fn_name(id);
        let cl_id = self.funcs[&name];
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
        self.define_common(prog, &name, cl_id, shape, &mut ctx, &mut fctx)
    }

    /// Lower one body behind the totality seatbelt, define it, and
    /// record its codegen time.
    fn define_common(
        &mut self,
        prog: &MProgram,
        name: &str,
        cl_id: ClFuncId,
        shape: BodyShape<'_>,
        ctx: &mut Context,
        fctx: &mut FunctionBuilderContext,
    ) -> Result<(), SubsetError> {
        let t0 = std::time::Instant::now();
        // Seatbelt: cranelift_frontend panics (rather than errors) on
        // malformed input such as use-before-def; checked MIR cannot
        // produce it, but the compiler stays total regardless.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut b = FunctionBuilder::new(&mut ctx.func, fctx);
            FnCx::lower(self, &mut b, prog, shape)?;
            let frontend_config = self.module.isa().frontend_config();
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
        self.module
            .define_function(cl_id, ctx)
            .map_err(|e| SubsetError {
                func: name.to_string(),
                message: format!("define failed: {e}"),
            })?;
        let dt = t0.elapsed().as_micros() as u64;
        self.timings.push((name.to_string(), dt));
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.funcs.contains_key(name)
    }

    pub fn ptr(&self, name: &str) -> *const u8 {
        self.module.get_finalized_function(self.funcs[name])
    }

    /// Finalized body entry points by body id (for harness wiring).
    pub fn body_entries(&self) -> Vec<(u32, *const u8)> {
        self.bodies
            .iter()
            .map(|(id, _, _)| (*id, self.ptr(&body_fn_name(*id))))
            .collect()
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

/// Per-body lowering state: MIR locals lower to Cranelift variables
/// (MIR is not SSA), handle-typed locals publish at safepoints, and
/// the shadow base scopes the epilogue.
pub(crate) struct FnCx<'a> {
    /// Error context: body name for `SubsetError`.
    func: String,
    mode: CodegenMode,
    ctx: CVal,
    locals: &'a [(String, Ty)],
    /// MIR local id → variable (all `I64`).
    vars: Vec<Variable>,
    /// Locals whose `Ty` is heap-managed (published at safepoints).
    handle_locals: HashMap<usize, ()>,
    /// MIR block id → CLIF block.
    blocks: HashMap<usize, Block>,
    /// Shadow depth at entry (epilogue pops to it).
    base: Option<CVal>,
    /// `FuncRef` resolutions seen so far: MIR local → program fn index.
    funcrefs: HashMap<usize, usize>,
    body_span: Span,
}

type CVal = cranelift_codegen::ir::Value;

impl Session for Jit {
    fn mode(&self) -> CodegenMode {
        self.mode
    }

    fn import(&mut self, name: &'static str, params: u32, returns: bool) -> ClFuncId {
        Jit::import(self, name, params, returns)
    }

    fn import_id(&self, name: &'static str) -> Option<ClFuncId> {
        self.imports.get(name).copied()
    }

    fn intern_str(&mut self, b: &mut FunctionBuilder, s: &str) -> (CVal, CVal) {
        let (ptr, len) = self.intern(s);
        let ptr_v = b.ins().iconst(I64, ptr as i64);
        let len_v = b.ins().iconst(I64, len as i64);
        (ptr_v, len_v)
    }

    fn declare_in_func(&mut self, id: ClFuncId, func: &mut Function) -> FuncRef {
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

impl<'a> FnCx<'a> {
    fn new(shape: &BodyShape<'a>, mode: CodegenMode, ctx: CVal) -> Self {
        Self {
            func: shape.name.to_string(),
            mode,
            ctx,
            locals: shape.locals,
            vars: Vec::new(),
            handle_locals: HashMap::new(),
            blocks: HashMap::new(),
            base: None,
            funcrefs: HashMap::new(),
            body_span: shape.body_span,
        }
    }

    fn fail<T>(&self, message: String) -> Result<T, SubsetError> {
        Err(SubsetError {
            func: self.func.clone(),
            message,
        })
    }

    fn local_ty(&self, local: usize) -> Result<&Ty, SubsetError> {
        self.locals
            .get(local)
            .map(|(_, ty)| ty)
            .ok_or_else(|| SubsetError {
                func: self.func.clone(),
                message: format!("local {local} out of range"),
            })
    }

    /// Trafficable types: immediates traffic as bit patterns, heap
    /// families as the handle codec, channels as scheduler ids,
    /// functions as indices or closure handles. Returns true for
    /// handle types (published at safepoints). `Fn` is conservatively
    /// a handle (it may hold a closure; plain indices publish
    /// harmlessly since they never resolve); channels are
    /// scheduler-owned and never published. `Map` is refused: it has
    /// values only as a type, never at runtime.
    fn trafficable(ty: &Ty) -> Result<bool, String> {
        match ty {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Unit | Ty::Chan(_) => Ok(false),
            // C scalars ride as immediates like `int`/`float`;
            // pointers are single-word addresses (never published).
            Ty::CChar | Ty::CInt | Ty::CLong | Ty::CDouble | Ty::CPtr(_) => Ok(false),
            Ty::String
            | Ty::Array(_)
            | Ty::Struct(_)
            | Ty::Range
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Fn(..) => Ok(true),
            Ty::Map(..) | Ty::CVoid => Err(format!("type {ty:?} has no runtime values")),
        }
    }

    fn iconst(b: &mut FunctionBuilder<'_>, v: i64) -> CVal {
        b.ins().iconst(I64, v)
    }

    /// Branch to a `toto_panic` block when `cond` is nonzero; control
    /// continues in a fresh block. Gate panics diverge there, task
    /// panics record and return a dummy word (the caller unwinds via
    /// the sentinel protocol). No trap instruction anywhere.
    fn emit_check(
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        ctx: CVal,
        cond: CVal,
        code: u64,
        span: (u64, u64),
        msg: &'static str,
    ) {
        // Through the session interner, never a baked address: the JIT
        // could use the binary's own `msg` address, but AOT objects
        // outlive the build process.
        let (msg_ptr, msg_len) = sess.intern_str(b, msg);
        Self::emit_check_raw(sess, b, ctx, cond, code, span, msg_ptr, msg_len);
    }

    /// `emit_check` with a baked message address (interned dynamic
    /// messages, e.g. type-mismatch wordings).
    #[allow(clippy::too_many_arguments)]
    fn emit_check_raw(
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        ctx: CVal,
        cond: CVal,
        code: u64,
        span: (u64, u64),
        msg_ptr: CVal,
        msg_len: CVal,
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
        let callee = sess.declare_in_func(
            sess.import_id("toto_panic")
                .expect("runtime import declared"),
            b.func,
        );
        let code_v = Self::iconst(b, code as i64);
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        b.ins()
            .call(callee, &[ctx, code_v, start_v, end_v, msg_ptr, msg_len]);
        // Unreachable at runtime (gate diverges, task unwinds from
        // here): return a dummy so the block is terminated without
        // any trap instruction.
        let dummy = Self::iconst(b, 0);
        let ret = [dummy];
        b.ins().return_(&ret);
        b.switch_to_block(cont_b);
        b.seal_block(cont_b);
    }

    /// Push every defined handle local (conservative publication).
    /// Handle variables are zero-initialized in the prologue, so every
    /// variable here is always defined: undefined-on-a-path locals
    /// publish the null word, which the collector skips (it never
    /// resolves).
    fn publish_all(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
    ) -> Result<(), SubsetError> {
        let mut ids: Vec<usize> = self.handle_locals.keys().copied().collect();
        ids.sort();
        for id in ids {
            let v = self.operand(b, id)?;
            self.call_import(sess, b, "toto_shadow_push", &[self.ctx, v])?;
        }
        Ok(())
    }

    /// Lower one body (multi-block): declare variables, emit the
    /// prologue (params, captures, shadow base), lower each block,
    /// terminate with the MIR terminators.
    pub(crate) fn lower(
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
        shape: BodyShape<'_>,
    ) -> Result<(), SubsetError> {
        Self::check_defined(&shape)?;
        for (i, (_, ty)) in shape.locals.iter().enumerate() {
            if let Err(e) = Self::trafficable(ty) {
                return Err(SubsetError {
                    func: shape.name.to_string(),
                    message: format!("local {i}: {e}"),
                });
            }
        }
        if Self::trafficable(shape.ret).is_err() {
            return Err(SubsetError {
                func: shape.name.to_string(),
                message: format!("return type {:?} has no runtime values", shape.ret),
            });
        }
        // Map MIR blocks to CLIF blocks (entry first: it carries the
        // function params).
        let entry_block = b.create_block();
        b.append_block_params_for_function_params(entry_block);
        let params = b.block_params(entry_block).to_vec();
        let ctx = params.first().copied().ok_or_else(|| SubsetError {
            func: shape.name.to_string(),
            message: String::from("missing ctx block parameter (unreachable)"),
        })?;
        let mut cx = FnCx::new(&shape, sess.mode(), ctx);
        cx.blocks.insert(shape.entry, entry_block);
        for blk in shape.blocks {
            if blk.id == shape.entry {
                continue;
            }
            if cx.blocks.contains_key(&blk.id) {
                return cx.fail(format!("duplicate block id {}", blk.id));
            }
            let cb = b.create_block();
            cx.blocks.insert(blk.id, cb);
        }
        // Declare one variable per local (index order = declaration
        // order, matching `cx.vars`).
        for _ in 0..shape.locals.len() {
            let var = b.declare_var(I64);
            cx.vars.push(var);
        }
        // Mark handle-typed locals for publication.
        for (i, (_, ty)) in shape.locals.iter().enumerate() {
            if Self::trafficable(ty).expect("validated above") {
                cx.handle_locals.insert(i, ());
            }
        }
        // Prologue in the entry block, then its body.
        b.switch_to_block(entry_block);
        cx.prologue(sess, b, &shape, &params)?;
        let entry_mir = shape
            .blocks
            .iter()
            .find(|blk| blk.id == shape.entry)
            .ok_or_else(|| SubsetError {
                func: shape.name.to_string(),
                message: String::from("entry block missing from body (unreachable)"),
            })?;
        for (i, instr) in entry_mir.instrs.iter().enumerate() {
            cx.lower_instr(sess, b, prog, &entry_mir.instrs, i, instr)?;
        }
        cx.lower_term(sess, b, prog, &shape, &entry_mir.term)?;
        for blk in shape.blocks {
            if blk.id == shape.entry {
                continue;
            }
            let cb = cx.blocks[&blk.id];
            b.switch_to_block(cb);
            for (i, instr) in blk.instrs.iter().enumerate() {
                cx.lower_instr(sess, b, prog, &blk.instrs, i, instr)?;
            }
            cx.lower_term(sess, b, prog, &shape, &blk.term)?;
        }
        // MIR block order is not topological (loop back-edges and
        // short-circuit joins jump to earlier blocks), so no MIR
        // block is sealed until every branch is emitted; internal
        // helper blocks stay eagerly sealed (self-contained).
        b.seal_all_blocks();
        Ok(())
    }

    /// Every local read anywhere in the body must be defined somewhere
    /// (params, capture slots, or an instruction destination). This
    /// catches malformed input cleanly; dominance itself holds by
    /// construction (checked MIR) and is guarded by the seatbelt.
    fn check_defined(shape: &BodyShape<'_>) -> Result<(), SubsetError> {
        let mut defined = vec![false; shape.locals.len()];
        for i in 0..shape.params.len() + shape.ncaptures {
            if i < defined.len() {
                defined[i] = true;
            }
        }
        for blk in shape.blocks {
            for instr in &blk.instrs {
                for d in Self::defs_of(instr) {
                    if d < defined.len() {
                        defined[d] = true;
                    }
                }
            }
        }
        let fail = |local: usize| SubsetError {
            func: shape.name.to_string(),
            message: format!("local {local} used before definition"),
        };
        for blk in shape.blocks {
            for instr in &blk.instrs {
                for u in Self::uses_of(instr) {
                    if u >= defined.len() || !defined[u] {
                        return Err(fail(u));
                    }
                }
            }
            for u in Self::term_uses(&blk.term) {
                if u >= defined.len() || !defined[u] {
                    return Err(fail(u));
                }
            }
        }
        Ok(())
    }

    /// Every local an instruction defines (destinations, split bounds,
    /// select binds — binds are defined by the lowering right after
    /// the suspending call, ahead of the dispatch branches).
    fn defs_of(instr: &MInstr) -> Vec<usize> {
        match instr {
            MInstr::Const { dst, .. }
            | MInstr::Copy { dst, .. }
            | MInstr::Bin { dst, .. }
            | MInstr::Neg { dst, .. }
            | MInstr::Not { dst, .. }
            | MInstr::FuncRef { dst, .. }
            | MInstr::StructLit { dst, .. }
            | MInstr::ArrayLit { dst, .. }
            | MInstr::Snapshot { dst, .. }
            | MInstr::RangeLit { dst, .. }
            | MInstr::WrapOk { dst, .. }
            | MInstr::WrapErr { dst, .. }
            | MInstr::WrapSome { dst, .. }
            | MInstr::Propagate { dst, .. }
            | MInstr::Field { dst, .. }
            | MInstr::Index { dst, .. }
            | MInstr::Len { dst, .. }
            | MInstr::SatIncr { dst, .. }
            | MInstr::MakeClosure { dst, .. }
            | MInstr::ChanNew { dst, .. }
            | MInstr::StateGet { dst, .. }
            | MInstr::Recv { dst, .. } => vec![*dst],
            MInstr::Call { dst, .. } | MInstr::CallBuiltin { dst, .. } => {
                dst.iter().copied().collect()
            }
            MInstr::CallForeign { dst, .. } => dst.iter().copied().collect(),
            MInstr::SplitRange { lo, hi, .. } => vec![*lo, *hi],
            MInstr::Select { dst, cases, .. } => {
                let mut out = vec![*dst];
                for case in cases {
                    if let MSelectCase::Recv { bind: Some(b), .. } = case {
                        out.push(*b);
                    }
                }
                out
            }
            MInstr::DivCheck { .. }
            | MInstr::Send { .. }
            | MInstr::Spawn { .. }
            | MInstr::StoreField { .. }
            | MInstr::StoreIndex { .. }
            | MInstr::StateSet { .. }
            | MInstr::Safepoint { .. }
            | MInstr::Cover { .. } => Vec::new(),
        }
    }

    /// Every local an instruction reads.
    fn uses_of(instr: &MInstr) -> Vec<usize> {
        match instr {
            MInstr::Copy { src, .. }
            | MInstr::Neg { operand: src, .. }
            | MInstr::Not { operand: src, .. }
            | MInstr::DivCheck { value: src, .. }
            | MInstr::Snapshot { base: src, .. }
            | MInstr::WrapOk { inner: src, .. }
            | MInstr::WrapErr { inner: src, .. }
            | MInstr::WrapSome { inner: src, .. }
            | MInstr::Propagate { operand: src, .. }
            | MInstr::Field { base: src, .. }
            | MInstr::Len { base: src, .. }
            | MInstr::SatIncr { operand: src, .. }
            | MInstr::Recv { chan: src, .. } => vec![*src],
            MInstr::Bin { l, r, .. } | MInstr::RangeLit { lo: l, hi: r, .. } => vec![*l, *r],
            MInstr::SplitRange { range, .. } => vec![*range],
            MInstr::Index { base, index, .. } => vec![*base, *index],
            MInstr::StoreField { base, value, .. } => vec![*base, *value],
            MInstr::StoreIndex {
                base, index, value, ..
            } => vec![*base, *index, *value],
            MInstr::Call { func, args, .. } => {
                let mut out = vec![*func];
                out.extend(args.iter().copied());
                out
            }
            MInstr::CallBuiltin { args, .. } => args.clone(),
            MInstr::CallForeign { args, .. } => args.clone(),
            MInstr::StructLit { fields, .. } => fields.iter().map(|(_, l)| *l).collect(),
            MInstr::ArrayLit { elems, .. } => elems.clone(),
            MInstr::MakeClosure { captures, .. } | MInstr::Spawn { captures, .. } => {
                captures.clone()
            }
            MInstr::ChanNew { cap, .. } => cap.iter().copied().collect(),
            MInstr::Send { chan, value, .. } => vec![*chan, *value],
            MInstr::Select { cases, .. } => {
                let mut out = Vec::new();
                for case in cases {
                    match case {
                        MSelectCase::Recv { chan, .. } => out.push(*chan),
                        MSelectCase::Send { chan, value, .. } => {
                            out.push(*chan);
                            out.push(*value);
                        }
                    }
                }
                out
            }
            MInstr::StateSet { src, .. } => vec![*src],
            MInstr::Const { .. }
            | MInstr::FuncRef { .. }
            | MInstr::StateGet { .. }
            | MInstr::Safepoint { .. }
            | MInstr::Cover { .. } => Vec::new(),
        }
    }

    fn term_uses(term: &MTerm) -> Vec<usize> {
        match term {
            MTerm::Return(Some(l)) => vec![*l],
            MTerm::Branch { cond, .. } => vec![*cond],
            MTerm::Return(None) | MTerm::FallOff { .. } | MTerm::Jump(_) => Vec::new(),
        }
    }

    /// Prologue: zero-initialize handle variables (total publication:
    /// the null word never resolves, so the collector skips it),
    /// define params and capture slots, record the shadow base.
    fn prologue(
        &mut self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        shape: &BodyShape<'_>,
        params: &[CVal],
    ) -> Result<(), SubsetError> {
        let zero = Self::iconst(b, 0);
        for id in self.handle_locals.keys() {
            b.def_var(self.vars[*id], zero);
        }
        let want = match shape.kind {
            BodyKind::TopLevel => shape.params.len() + 1,
            BodyKind::Closure => shape.params.len() + 2,
            BodyKind::Go => 1,
        };
        if params.len() != want {
            return self.fail(format!(
                "block params {} vs {want} expected (unreachable)",
                params.len()
            ));
        }
        // Record the shadow base BEFORE the capture fetches below:
        // they end in `check_silent`, which needs the base to unwind.
        // Capture fetches never push or pop shadows, so reading the
        // depth here is identical to reading it after them.
        let base = self.call_value(sess, b, "toto_shadow_len", &[self.ctx])?;
        self.base = Some(base);
        match shape.kind {
            BodyKind::TopLevel => {
                for (i, v) in params.iter().skip(1).enumerate() {
                    self.define(b, i, *v)?;
                }
            }
            BodyKind::Closure => {
                let handle = params[1];
                for (i, v) in params.iter().skip(2).enumerate() {
                    self.define(b, i, *v)?;
                }
                for i in 0..shape.ncaptures {
                    let slot = shape.params.len() + i;
                    let w = self.call_closure_cap(sess, b, handle, i, shape.body_span)?;
                    self.define(b, slot, w)?;
                    self.check_silent(sess, b, shape.body_span)?;
                }
            }
            BodyKind::Go => {
                for i in 0..shape.ncaptures {
                    let w = self.call_task_capture(sess, b, i, shape.body_span)?;
                    self.define(b, i, w)?;
                    self.check_silent(sess, b, shape.body_span)?;
                }
            }
        }
        Ok(())
    }

    /// Fetch one closure capture word (handle payloads).
    fn call_closure_cap(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        handle: CVal,
        idx: usize,
        span: Span,
    ) -> Result<CVal, SubsetError> {
        let i = Self::iconst(b, idx as i64);
        let start = Self::iconst(b, span.start as i64);
        let end = Self::iconst(b, span.end as i64);
        self.call_value(
            sess,
            b,
            "toto_closure_cap",
            &[self.ctx, handle, i, start, end],
        )
    }

    /// Fetch one go-body capture word (task context slots).
    fn call_task_capture(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        idx: usize,
        span: Span,
    ) -> Result<CVal, SubsetError> {
        let i = Self::iconst(b, idx as i64);
        let start = Self::iconst(b, span.start as i64);
        let end = Self::iconst(b, span.end as i64);
        self.call_value(sess, b, "toto_task_capture", &[self.ctx, i, start, end])
    }

    /// Epilogue: pop the shadow stack to the prologue-recorded base.
    fn epilogue(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
    ) -> Result<(), SubsetError> {
        let base = self.base.ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: String::from("missing shadow base (unreachable)"),
        })?;
        self.call_import(sess, b, "toto_shadow_pop_to", &[self.ctx, base])?;
        Ok(())
    }

    /// Lower a block terminator (all MIR terminators).
    fn lower_term(
        &mut self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
        shape: &BodyShape<'_>,
        term: &MTerm,
    ) -> Result<(), SubsetError> {
        use cranelift_codegen::ir::condcodes::IntCC;
        let _ = prog;
        match term {
            MTerm::Return(Some(local)) => {
                let v = self.operand(b, *local)?;
                self.epilogue(sess, b)?;
                b.ins().return_(&[v]);
                Ok(())
            }
            MTerm::Return(None) => {
                self.epilogue(sess, b)?;
                let z = Self::iconst(b, 0);
                b.ins().return_(&[z]);
                Ok(())
            }
            MTerm::FallOff { .. } => {
                if *shape.ret == Ty::Unit {
                    self.epilogue(sess, b)?;
                    let z = Self::iconst(b, 0);
                    b.ins().return_(&[z]);
                    Ok(())
                } else {
                    let msg = match shape.kind {
                        BodyKind::TopLevel => {
                            format!("function `{}` exited without returning a value", shape.name)
                        }
                        BodyKind::Closure => {
                            String::from("closure exited without returning a value")
                        }
                        BodyKind::Go => {
                            return self.fail(String::from(
                                "go body fell off a non-unit return (unreachable)",
                            ));
                        }
                    };
                    self.emit_terminal_panic(
                        sess,
                        b,
                        E_FALLOFF,
                        (shape.body_span.start as u64, shape.body_span.end as u64),
                        &msg,
                    )
                }
            }
            MTerm::Jump(target) => {
                let cb = self
                    .blocks
                    .get(target)
                    .copied()
                    .ok_or_else(|| SubsetError {
                        func: self.func.clone(),
                        message: format!("jump to missing block {target}"),
                    })?;
                b.ins().jump(cb, &[]);
                Ok(())
            }
            MTerm::Branch {
                cond,
                then_b,
                else_b,
                ..
            } => {
                let c = self.operand(b, *cond)?;
                // Boolean words are 0/1 by construction (all producers
                // emit them; checked code never branches on anything
                // else), so no `E0207` backstop is emitted.
                let zero = Self::iconst(b, 0);
                let t = b.ins().icmp(IntCC::NotEqual, c, zero);
                let then_c = self
                    .blocks
                    .get(then_b)
                    .copied()
                    .ok_or_else(|| SubsetError {
                        func: self.func.clone(),
                        message: format!("branch to missing block {then_b}"),
                    })?;
                let else_c = self
                    .blocks
                    .get(else_b)
                    .copied()
                    .ok_or_else(|| SubsetError {
                        func: self.func.clone(),
                        message: format!("branch to missing block {else_b}"),
                    })?;
                b.ins().brif(t, then_c, &[], else_c, &[]);
                Ok(())
            }
        }
    }

    /// Unconditional panic that ends the current block (fall-off and
    /// other statically-dead failures): calls `toto_panic`, then a
    /// dummy return. No continuation block — the caller must be at a
    /// block end.
    fn emit_terminal_panic(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        code: u64,
        span: (u64, u64),
        msg: &str,
    ) -> Result<(), SubsetError> {
        let code_v = Self::iconst(b, code as i64);
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        let (msg_ptr, msg_len) = sess.intern_str(b, msg);
        self.call_import(
            sess,
            b,
            "toto_panic",
            &[self.ctx, code_v, start_v, end_v, msg_ptr, msg_len],
        )?;
        let dummy = Self::iconst(b, 0);
        b.ins().return_(&[dummy]);
        Ok(())
    }

    /// Unconditional mid-block panic (statically-dead failures):
    /// always-firing check plus a dummy word the caller defines in
    /// the dead continuation so the function stays well-formed.
    fn emit_dead_panic(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        code: u64,
        span: (u64, u64),
        msg: &str,
    ) -> Result<CVal, SubsetError> {
        let one = Self::iconst(b, 1);
        let (msg_ptr, msg_len) = sess.intern_str(b, msg);
        Self::emit_check_raw(sess, b, self.ctx, one, code, span, msg_ptr, msg_len);
        Ok(Self::iconst(b, 0))
    }

    /// Sentinel check after a Toto-level call (task mode only): on
    /// failure the callee frame (`name`, call-site span — the caller
    /// knows both statically, exactly like the executor pushing at
    /// call time) is appended and generated code unwinds by returning
    /// a dummy word immediately. Gate mode needs no check (diverging
    /// panics). Empty names skip the append (runtime helpers are not
    /// Toto frames).
    #[allow(clippy::too_many_arguments)]
    fn emit_check_failed(
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        ctx: CVal,
        base: CVal,
        name_ptr: CVal,
        name_len: CVal,
        span: (u64, u64),
    ) {
        use cranelift_codegen::ir::condcodes::IntCC;
        let callee = sess.declare_in_func(
            sess.import_id("toto_check_failed")
                .expect("runtime import declared"),
            b.func,
        );
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        let inst = b
            .ins()
            .call(callee, &[ctx, name_ptr, name_len, start_v, end_v]);
        let failed = b.inst_results(inst)[0];
        let zero = Self::iconst(b, 0);
        let is_failed = b.ins().icmp(IntCC::NotEqual, failed, zero);
        let fail_b = b.create_block();
        let cont_b = b.create_block();
        b.ins().brif(is_failed, fail_b, &[], cont_b, &[]);
        b.switch_to_block(fail_b);
        b.seal_block(fail_b);
        let pop = sess.declare_in_func(
            sess.import_id("toto_shadow_pop_to")
                .expect("runtime import declared"),
            b.func,
        );
        b.ins().call(pop, &[ctx, base]);
        let dummy = Self::iconst(b, 0);
        b.ins().return_(&[dummy]);
        b.switch_to_block(cont_b);
        b.seal_block(cont_b);
    }

    /// Sentinel check after a runtime-helper call (task mode only):
    /// unwinds explicitly on failure without appending a frame.
    /// Gate mode needs no check (diverging panics).
    fn check_silent(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        span: Span,
    ) -> Result<(), SubsetError> {
        if self.mode == CodegenMode::Gate {
            return Ok(());
        }
        let base = self.base.ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: String::from("missing shadow base (unreachable)"),
        })?;
        let z = Self::iconst(b, 0);
        Self::emit_check_failed(
            sess,
            b,
            self.ctx,
            base,
            z,
            z,
            (span.start as u64, span.end as u64),
        );
        Ok(())
    }

    /// Sentinel check after a Toto-level call (task mode only):
    /// appends the callee frame (`name`, call-site span — the caller
    /// knows both statically, exactly like the executor pushing at
    /// call time) and unwinds on failure. Gate mode needs no check
    /// (diverging panics).
    fn check_named(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        name: &str,
        span: Span,
    ) -> Result<(), SubsetError> {
        if self.mode == CodegenMode::Gate {
            return Ok(());
        }
        let base = self.base.ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: String::from("missing shadow base (unreachable)"),
        })?;
        let (name_ptr, name_len) = sess.intern_str(b, name);
        Self::emit_check_failed(
            sess,
            b,
            self.ctx,
            base,
            name_ptr,
            name_len,
            (span.start as u64, span.end as u64),
        );
        Ok(())
    }

    /// Direct call to a statically resolved top-level function.
    fn lower_direct_call(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
        target: usize,
        argvals: &[CVal],
        span: &Span,
    ) -> Result<CVal, SubsetError> {
        let callee_fn = prog.fns.get(target).ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("funcref index {target} out of range"),
        })?;
        if callee_fn.params.len() != argvals.len() {
            return self.fail(format!(
                "call arity {} vs {} params of {} (unreachable: checked code)",
                argvals.len(),
                callee_fn.params.len(),
                callee_fn.name
            ));
        }
        let cl_id = sess.func_id(&callee_fn.name).ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("callee {} not compiled", callee_fn.name),
        })?;
        let callee = sess.declare_in_func(cl_id, b.func);
        let mut vals = vec![self.ctx];
        vals.extend(argvals.iter().copied());
        let inst = b.ins().call(callee, &vals);
        let ret = b.inst_results(inst)[0];
        self.check_named(sess, b, &callee_fn.name, *span)?;
        Ok(ret)
    }

    /// Indirect call through a `fn`-typed value: words below 2^32
    /// call top-level functions, the rest call closures with the
    /// value as their handle argument. The callee frame carries the
    /// source display name (or `<value>`), like the interpreters.
    fn lower_indirect_call(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        func: usize,
        argvals: &[CVal],
        span: &Span,
        display: &str,
    ) -> Result<CVal, SubsetError> {
        use cranelift_codegen::ir::condcodes::IntCC;
        let fty = self.local_ty(func)?.clone();
        if !matches!(fty, Ty::Fn(..)) {
            let msg = format!(
                "call of non-function value ({})",
                Self::static_type_name(&fty)
            );
            return self.emit_dead_panic(
                sess,
                b,
                E_INTERNAL,
                (span.start as u64, span.end as u64),
                &msg,
            );
        }
        let fv = self.operand(b, func)?;
        let bound = Self::iconst(b, 0x1_0000_0000);
        let is_func = b.ins().icmp(IntCC::UnsignedLessThan, fv, bound);
        let func_b = b.create_block();
        let clo_b = b.create_block();
        let merge_b = b.create_block();
        b.append_block_param(merge_b, I64);
        b.ins().brif(is_func, func_b, &[], clo_b, &[]);
        // Index arm: resolve and call `(ctx, args...)`.
        b.switch_to_block(func_b);
        b.seal_block(func_b);
        let fstart = Self::iconst(b, span.start as i64);
        let fend = Self::iconst(b, span.end as i64);
        let entry = self.call_value(sess, b, "toto_func_entry", &[self.ctx, fv, fstart, fend])?;
        self.check_silent(sess, b, *span)?;
        let fsig = sig_arity(argvals.len() as u32, true);
        let fsig = b.func.import_signature(fsig);
        let mut fvals = vec![self.ctx];
        fvals.extend(argvals.iter().copied());
        let finst = b.ins().call_indirect(fsig, entry, &fvals);
        let fret = b.inst_results(finst)[0];
        self.check_named(sess, b, display, *span)?;
        b.ins().jump(merge_b, &[BlockArg::Value(fret)]);
        // Closure arm: resolve the body and call `(ctx, handle, args...)`.
        b.switch_to_block(clo_b);
        b.seal_block(clo_b);
        let cstart = Self::iconst(b, span.start as i64);
        let cend = Self::iconst(b, span.end as i64);
        let body = self.call_value(sess, b, "toto_closure_body", &[self.ctx, fv, cstart, cend])?;
        self.check_silent(sess, b, *span)?;
        let centry =
            self.call_value(sess, b, "toto_body_entry", &[self.ctx, body, cstart, cend])?;
        self.check_silent(sess, b, *span)?;
        let csig = sig_arity(argvals.len() as u32 + 1, true);
        let csig = b.func.import_signature(csig);
        let mut cvals = vec![self.ctx, fv];
        cvals.extend(argvals.iter().copied());
        let cinst = b.ins().call_indirect(csig, centry, &cvals);
        let cret = b.inst_results(cinst)[0];
        self.check_named(sess, b, display, *span)?;
        b.ins().jump(merge_b, &[BlockArg::Value(cret)]);
        b.switch_to_block(merge_b);
        b.seal_block(merge_b);
        Ok(b.block_params(merge_b)[0])
    }

    /// Call an imported helper, returning its first result (if any).
    /// Missing imports (a task-only import in gate mode) refuse
    /// cleanly instead of panicking on the map lookup.
    fn call_import(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        name: &'static str,
        args: &[CVal],
    ) -> Result<Option<CVal>, SubsetError> {
        let id = sess.import_id(name).ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("import `{name}` not registered for this mode"),
        })?;
        let callee = sess.declare_in_func(id, b.func);
        let inst = b.ins().call(callee, args);
        Ok(b.inst_results(inst).first().copied())
    }

    /// Call an imported helper that returns a word.
    fn call_value(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        name: &'static str,
        args: &[CVal],
    ) -> Result<CVal, SubsetError> {
        self.call_import(sess, b, name, args)?
            .ok_or_else(|| SubsetError {
                func: self.func.clone(),
                message: format!("import `{name}` returned no value (unreachable)"),
            })
    }

    /// A tagged operand pair (static tag iconst + word) for helpers.
    fn tagged_operand(
        &self,
        b: &mut FunctionBuilder<'_>,
        local: usize,
    ) -> Result<(CVal, CVal), SubsetError> {
        let ty = self.local_ty(local)?.clone();
        let tag = Self::tag_of(&ty).map_err(|e| SubsetError {
            func: self.func.clone(),
            message: format!("operand {local}: {e}"),
        })?;
        let word = self.operand(b, local)?;
        Ok((Self::iconst(b, tag as i64), word))
    }

    /// Allocate a writable stack slot of `bytes` (8-byte aligned),
    /// returning its address.
    fn stack_slot(b: &mut FunctionBuilder<'_>, bytes: u32) -> CVal {
        let slot =
            b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, bytes, 3));
        b.ins().stack_addr(I64, slot, 0)
    }

    /// Store `v` at `base + idx * 8`.
    fn stack_store(b: &mut FunctionBuilder<'_>, base: CVal, idx: usize, v: CVal) {
        b.ins()
            .store(MemFlagsData::trusted(), v, base, (idx * 8) as i32);
    }

    /// Load the word at `base + idx * 8`.
    fn stack_load(b: &mut FunctionBuilder<'_>, base: CVal, idx: usize) -> CVal {
        b.ins()
            .load(I64, MemFlagsData::trusted(), base, (idx * 8) as i32)
    }

    fn operand(&self, b: &mut FunctionBuilder<'_>, local: usize) -> Result<CVal, SubsetError> {
        let var = self.vars.get(local).copied().ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("local {local} out of range"),
        })?;
        Ok(b.use_var(var))
    }

    fn define(
        &self,
        b: &mut FunctionBuilder<'_>,
        local: usize,
        v: CVal,
    ) -> Result<(), SubsetError> {
        let var = self.vars.get(local).copied().ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: format!("local {local} out of range"),
        })?;
        b.def_var(var, v);
        Ok(())
    }

    /// Structured branch returning a value (both arms join with a
    /// block param). `cond` is a raw comparison result, like Spike 3's
    /// `brif` use.
    fn if_else<S: Session>(
        sess: &mut S,
        b: &mut FunctionBuilder<'_>,
        cond: CVal,
        then_branch: impl FnOnce(&mut S, &mut FunctionBuilder<'_>) -> CVal,
        else_branch: impl FnOnce(&mut S, &mut FunctionBuilder<'_>) -> CVal,
    ) -> CVal {
        let then_b = b.create_block();
        let else_b = b.create_block();
        let merge_b = b.create_block();
        b.append_block_param(merge_b, I64);
        b.ins().brif(cond, then_b, &[], else_b, &[]);
        b.switch_to_block(then_b);
        b.seal_block(then_b);
        let v1 = then_branch(sess, b);
        b.ins().jump(merge_b, &[BlockArg::Value(v1)]);
        b.switch_to_block(else_b);
        b.seal_block(else_b);
        let v0 = else_branch(sess, b);
        b.ins().jump(merge_b, &[BlockArg::Value(v0)]);
        b.switch_to_block(merge_b);
        b.seal_block(merge_b);
        b.block_params(merge_b)[0]
    }

    /// Float arithmetic on word traffic: operands arrive as `I64`
    /// bit patterns (frozen single-word `u64` traffic), so bitcast
    /// to `F64` for the operation and back to `I64` for the result.
    /// `op` selects add/sub/mul/div in that order (IEEE semantics,
    /// exactly like the interpreter: no traps, NaN propagates).
    fn farith(b: &mut FunctionBuilder<'_>, op: u8, a: CVal, c: CVal) -> CVal {
        let flags = MemFlagsData::new().with_endianness(Endianness::Little);
        let x = b.ins().bitcast(F64, flags, a);
        let y = b.ins().bitcast(F64, flags, c);
        let r = match op {
            0 => b.ins().fadd(x, y),
            1 => b.ins().fsub(x, y),
            2 => b.ins().fmul(x, y),
            _ => b.ins().fdiv(x, y),
        };
        b.ins().bitcast(I64, flags, r)
    }

    /// Float comparison on word traffic: bitcast both operands to
    /// `F64` first (same word convention as [`Self::farith`]).
    fn fcmp_word(
        b: &mut FunctionBuilder<'_>,
        cc: cranelift_codegen::ir::condcodes::FloatCC,
        a: CVal,
        c: CVal,
    ) -> CVal {
        let flags = MemFlagsData::new().with_endianness(Endianness::Little);
        let x = b.ins().bitcast(F64, flags, a);
        let y = b.ins().bitcast(F64, flags, c);
        let e = b.ins().fcmp(cc, x, y);
        b.ins().uextend(I64, e)
    }

    /// Integer arithmetic with the frozen explicit checks (`E0201`
    /// overflow, `E0202` zero divisor with the `DivCheck` span,
    /// `E0209` is handled at `Index`). Semantics mirror `ops`
    /// exactly, including `MIN/-1` division overflow and
    /// `wrapping_rem` for `%`. Same-type pairs the interpreter
    /// rejects panic `E0203` unconditionally; cross-type pairs
    /// (checker-impossible) do the same through the helpers.
    #[allow(clippy::too_many_arguments)]
    fn lower_bin(
        &mut self,
        sess: &mut impl Session,
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
                Self::emit_check(sess, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
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
                Self::emit_check(sess, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
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
                    sess,
                    b,
                    e_a0,
                    |_, b| Self::iconst(b, 0),
                    |sess, b| {
                        Self::if_else(
                            sess,
                            b,
                            e_c0,
                            |_, b| Self::iconst(b, 0),
                            |sess, b| {
                                Self::if_else(
                                    sess,
                                    b,
                                    e_an1,
                                    |_, b| {
                                        let e = b.ins().icmp(IntCC::Equal, c, min);
                                        b.ins().uextend(I64, e)
                                    },
                                    |sess, b| {
                                        Self::if_else(
                                            sess,
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
                Self::emit_check(sess, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(r)
            }
            (MBinOp::Div, Ty::Int, Ty::Int) => {
                let zero = Self::iconst(b, 0);
                let z = b.ins().icmp(IntCC::Equal, c, zero);
                let z64 = b.ins().uextend(I64, z);
                Self::emit_check(
                    sess,
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
                Self::emit_check(sess, b, self.ctx, ov, E_OVERFLOW, span, MSG_OVERFLOW);
                Ok(b.ins().sdiv(a, c))
            }
            (MBinOp::Mod, Ty::Int, Ty::Int) => {
                let zero = Self::iconst(b, 0);
                let z = b.ins().icmp(IntCC::Equal, c, zero);
                let z64 = b.ins().uextend(I64, z);
                Self::emit_check(
                    sess,
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
                    sess,
                    b,
                    special,
                    |_, b| Self::iconst(b, 0),
                    |_, b| b.ins().srem(a, c),
                ))
            }
            (MBinOp::Add, Ty::Float, Ty::Float) => Ok(Self::farith(b, 0, a, c)),
            (MBinOp::Sub, Ty::Float, Ty::Float) => Ok(Self::farith(b, 1, a, c)),
            (MBinOp::Mul, Ty::Float, Ty::Float) => Ok(Self::farith(b, 2, a, c)),
            (MBinOp::Div, Ty::Float, Ty::Float) => Ok(Self::farith(b, 3, a, c)),
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
            (MBinOp::Eq, Ty::Float, Ty::Float) => Ok(Self::fcmp_word(b, FloatCC::Equal, a, c)),
            (MBinOp::NotEq, Ty::Float, Ty::Float) => {
                Ok(Self::fcmp_word(b, FloatCC::NotEqual, a, c))
            }
            (MBinOp::Lt, Ty::Float, Ty::Float) => Ok(Self::fcmp_word(b, FloatCC::LessThan, a, c)),
            (MBinOp::LtEq, Ty::Float, Ty::Float) => {
                Ok(Self::fcmp_word(b, FloatCC::LessThanOrEqual, a, c))
            }
            (MBinOp::Gt, Ty::Float, Ty::Float) => {
                Ok(Self::fcmp_word(b, FloatCC::GreaterThan, a, c))
            }
            (MBinOp::GtEq, Ty::Float, Ty::Float) => {
                Ok(Self::fcmp_word(b, FloatCC::GreaterThanOrEqual, a, c))
            }
            // Boolean equality is trivially inline.
            (MBinOp::Eq, Ty::Bool, Ty::Bool) => {
                let e = b.ins().icmp(IntCC::Equal, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::NotEq, Ty::Bool, Ty::Bool) => {
                let e = b.ins().icmp(IntCC::NotEqual, a, c);
                Ok(b.ins().uextend(I64, e))
            }
            (MBinOp::Eq, Ty::Unit, Ty::Unit) => Ok(Self::iconst(b, 1)),
            (MBinOp::NotEq, Ty::Unit, Ty::Unit) => Ok(Self::iconst(b, 0)),
            // String concatenation runs in a helper (heap reads).
            (MBinOp::Add, Ty::String, Ty::String) => {
                let start_v = Self::iconst(b, span.0 as i64);
                let end_v = Self::iconst(b, span.1 as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_str_concat",
                    &[self.ctx, a, c, start_v, end_v],
                )?;
                self.check_silent(sess, b, Span::new(span.0 as u32, span.1 as u32))?;
                Ok(v)
            }
            // All other equality/ordering: the helpers implement the
            // exact interpreter rule (including cross-type `E0203`).
            (MBinOp::Eq, _, _) => self.lower_eq(sess, b, lty, rty, a, c, span, true),
            (MBinOp::NotEq, _, _) => self.lower_eq(sess, b, lty, rty, a, c, span, false),
            (MBinOp::Lt, _, _) => self.lower_order(sess, b, 0, lty, rty, a, c, span),
            (MBinOp::LtEq, _, _) => self.lower_order(sess, b, 1, lty, rty, a, c, span),
            (MBinOp::Gt, _, _) => self.lower_order(sess, b, 2, lty, rty, a, c, span),
            (MBinOp::GtEq, _, _) => self.lower_order(sess, b, 3, lty, rty, a, c, span),
            // Remaining arithmetic (same-type-unsupported and
            // cross-type): unconditional `E0203` with static names.
            (MBinOp::Add | MBinOp::Sub | MBinOp::Mul | MBinOp::Div | MBinOp::Mod, _, _) => {
                self.lower_arith_mismatch(sess, b, op, lty, rty, span)
            }
        }
    }

    /// Runtime type name of a static type (for statically-worded
    /// `E0203` messages; mirrors `type_name` exactly, including
    /// `struct`/`function` anonymity).
    fn static_type_name(ty: &Ty) -> &'static str {
        match ty {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::String => "string",
            Ty::Bool => "bool",
            Ty::Unit => "unit",
            Ty::Range => "Range",
            Ty::Array(_) => "Array",
            Ty::Struct(_) => "struct",
            Ty::Chan(_) => "chan",
            Ty::Fn(..) => "function",
            Ty::Result(..) => "Result",
            Ty::Option(_) => "Option",
            Ty::Map(..) => "Map",
            // C ABI spellings (`Ty::Display`); `*T` mirrors the runtime
            // `pointer` name for `Ptr` values. Unreachable: the JIT
            // refuses FFI programs before codegen.
            Ty::CChar => "c_char",
            Ty::CInt => "c_int",
            Ty::CLong => "c_long",
            Ty::CDouble => "c_double",
            Ty::CVoid => "c_void",
            Ty::CPtr(_) => "pointer",
        }
    }

    /// Statically-unsupported arithmetic: unconditional `E0203` with
    /// static type names (correct whenever reached: same-type pairs
    /// the interpreter rejects, or checker-impossible mixes). The
    /// caller defines the destination in the dead continuation so the
    /// function stays well-formed.
    fn lower_arith_mismatch(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        op: MBinOp,
        lty: &Ty,
        rty: &Ty,
        span: (u64, u64),
    ) -> Result<CVal, SubsetError> {
        let a = Self::static_type_name(lty);
        let c = Self::static_type_name(rty);
        let msg = match op {
            MBinOp::Add => format!("cannot add {a} and {c}"),
            MBinOp::Sub => format!("cannot subtract {a} and {c}"),
            MBinOp::Mul => format!("cannot multiply {a} and {c}"),
            MBinOp::Div => format!("cannot divide {a} by {c}"),
            MBinOp::Mod => format!("cannot take modulo of {a} and {c}"),
            _ => {
                return self.fail(format!("{op:?} is not arithmetic (unreachable)"));
            }
        };
        let one = Self::iconst(b, 1);
        let (msg_ptr, msg_len) = sess.intern_str(b, &msg);
        Self::emit_check_raw(sess, b, self.ctx, one, E_MISMATCH, span, msg_ptr, msg_len);
        Ok(Self::iconst(b, 0))
    }

    /// `==`/`!=` past the inline fast paths: the helper implements
    /// the exact interpreter rule (same-type check, deep comparison,
    /// cross-type `E0203` with hint).
    #[allow(clippy::too_many_arguments)]
    fn lower_eq(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        lty: &Ty,
        rty: &Ty,
        a: CVal,
        c: CVal,
        span: (u64, u64),
        want_eq: bool,
    ) -> Result<CVal, SubsetError> {
        let ta = Self::tag_of(lty).map_err(|e| SubsetError {
            func: self.func.clone(),
            message: format!("equality operand: {e}"),
        })?;
        let tb = Self::tag_of(rty).map_err(|e| SubsetError {
            func: self.func.clone(),
            message: format!("equality operand: {e}"),
        })?;
        let tag_a = Self::iconst(b, ta as i64);
        let tag_b = Self::iconst(b, tb as i64);
        let want = Self::iconst(b, i64::from(want_eq));
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        let v = self.call_value(
            sess,
            b,
            "toto_values_eq",
            &[self.ctx, tag_a, a, tag_b, c, want, start_v, end_v],
        )?;
        self.check_silent(sess, b, Span::new(span.0 as u32, span.1 as u32))?;
        Ok(v)
    }

    /// `<`/`<=`/`>`/`>=` past the inline fast paths: the helper
    /// implements the exact interpreter rule. `op` is 0/1/2/3 for
    /// `Lt`/`LtEq`/`Gt`/`GtEq`.
    #[allow(clippy::too_many_arguments)]
    fn lower_order(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        op: u64,
        lty: &Ty,
        rty: &Ty,
        a: CVal,
        c: CVal,
        span: (u64, u64),
    ) -> Result<CVal, SubsetError> {
        let ta = Self::tag_of(lty).map_err(|e| SubsetError {
            func: self.func.clone(),
            message: format!("ordering operand: {e}"),
        })?;
        let tb = Self::tag_of(rty).map_err(|e| SubsetError {
            func: self.func.clone(),
            message: format!("ordering operand: {e}"),
        })?;
        let op_v = Self::iconst(b, op as i64);
        let tag_a = Self::iconst(b, ta as i64);
        let tag_b = Self::iconst(b, tb as i64);
        let start_v = Self::iconst(b, span.0 as i64);
        let end_v = Self::iconst(b, span.1 as i64);
        let v = self.call_value(
            sess,
            b,
            "toto_values_order",
            &[self.ctx, op_v, tag_a, a, tag_b, c, start_v, end_v],
        )?;
        self.check_silent(sess, b, Span::new(span.0 as u32, span.1 as u32))?;
        Ok(v)
    }

    /// Deterministic foreign-function index: position in the
    /// name-sorted extern table. Runtime contexts build the same
    /// table at install time, so codegen and runtime agree without
    /// threading order-sensitive state through lowering (HashMap
    /// iteration order would bake nondeterminism into objects).
    fn extern_index(prog: &MProgram, name: &str) -> Option<u64> {
        let mut names: Vec<&str> = prog.externs.keys().map(String::as_str).collect();
        names.sort_unstable();
        names.iter().position(|n| *n == name).map(|i| i as u64)
    }

    /// Transport tag for a MIR-typed word, by static type.
    /// Dynamically-variant families resolve at runtime: `Option`
    /// words use `TAG_HANDLE` (word 0 decodes as `None`), `fn` words
    /// use `TAG_FN_ANY` (indices vs closure handles by range). C
    /// scalars ride their Toto-view tags (`c_int` as `TAG_INT`,
    /// `c_double` as `TAG_FLOAT`, pointers as `TAG_PTR`), matching
    /// the executor's checking-time view rule.
    fn tag_of(ty: &Ty) -> Result<u64, String> {
        match ty {
            Ty::Int => Ok(TAG_INT),
            Ty::Float => Ok(TAG_FLOAT),
            Ty::Bool => Ok(TAG_BOOL),
            Ty::String
            | Ty::Array(_)
            | Ty::Struct(_)
            | Ty::Range
            | Ty::Result(..)
            | Ty::Option(_) => Ok(TAG_HANDLE),
            Ty::Unit => Ok(TAG_UNIT),
            Ty::Chan(_) => Ok(TAG_CHAN),
            Ty::Fn(..) => Ok(TAG_FN_ANY),
            Ty::Map(..) | Ty::CVoid => Err(format!("type {ty:?} has no runtime values")),
            Ty::CChar | Ty::CInt | Ty::CLong => Ok(TAG_INT),
            Ty::CDouble => Ok(TAG_FLOAT),
            Ty::CPtr(_) => Ok(TAG_PTR),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_instr(
        &mut self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        prog: &MProgram,
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
                        // Words traffic as single-word `u64` (frozen D8):
                        // floats are bit patterns, never native `F64`.
                        Self::iconst(b, f.to_bits() as i64)
                    }
                    MConst::Str(s) => {
                        // JIT-owned bytes (freed with the session).
                        // Publish first: other live handles must
                        // survive the threshold check inside the
                        // allocator.
                        self.publish_all(sess, b)?;
                        let (ptr_v, len_v) = sess.intern_str(b, s);
                        // Span of a const: no meaningful span exists;
                        // allocation itself cannot fail on valid UTF-8
                        // (interned from a checked literal), so zeros.
                        let z0 = Self::iconst(b, 0);
                        let z1 = Self::iconst(b, 0);
                        let v = self.call_value(
                            sess,
                            b,
                            "toto_alloc_str",
                            &[self.ctx, ptr_v, len_v, z0, z1],
                        )?;
                        self.check_silent(sess, b, Span::new(0, 0))?;
                        v
                    }
                };
                self.define(b, *dst, v)?;
            }
            MInstr::Copy { dst, src } => {
                let v = self.operand(b, *src)?;
                self.define(b, *dst, v)?;
            }
            MInstr::Bin {
                dst,
                op,
                l,
                r,
                span,
            } => {
                let lty = self.local_ty(*l)?.clone();
                let rty = self.local_ty(*r)?.clone();
                // Integer `Div`/`Mod` must be paired with an
                // immediately preceding `DivCheck` on the divisor
                // (Tier-1 pins this structurally; the backend verifies
                // instead of trusting, and reports the check's span on
                // failure — exactly like the interpreters, where
                // `DivCheck` fires first).
                let divcheck_span = if matches!(op, MBinOp::Div | MBinOp::Mod)
                    && matches!(lty, Ty::Int)
                    && matches!(rty, Ty::Int)
                {
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
                let a = self.operand(b, *l)?;
                let c = self.operand(b, *r)?;
                let v = self.lower_bin(
                    sess,
                    b,
                    *op,
                    a,
                    c,
                    &lty,
                    &rty,
                    (span.start as u64, span.end as u64),
                    divcheck_span,
                )?;
                self.define(b, *dst, v)?;
            }
            MInstr::Neg { dst, operand, span } => {
                let v = self.operand(b, *operand)?;
                let ty = self.local_ty(*operand)?.clone();
                let r = match ty {
                    Ty::Int => {
                        let min = Self::iconst(b, i64::MIN);
                        let is_min =
                            b.ins()
                                .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, v, min);
                        let is_min64 = b.ins().uextend(I64, is_min);
                        Self::emit_check(
                            sess,
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
                    Ty::Float => {
                        let flags = MemFlagsData::new().with_endianness(Endianness::Little);
                        let x = b.ins().bitcast(F64, flags, v);
                        let r = b.ins().fneg(x);
                        b.ins().bitcast(I64, flags, r)
                    }
                    other => {
                        // Statically impossible (the checker rejects
                        // negation of non-numbers), panicking
                        // faithfully if ever reached.
                        let msg = format!("cannot negate {}", Self::static_type_name(&other));
                        self.emit_dead_panic(
                            sess,
                            b,
                            E_MISMATCH,
                            (span.start as u64, span.end as u64),
                            &msg,
                        )?
                    }
                };
                self.define(b, *dst, r)?;
            }
            MInstr::Not { dst, operand, .. } => {
                let v = self.operand(b, *operand)?;
                let ty = self.local_ty(*operand)?.clone();
                match ty {
                    Ty::Bool => {
                        let one = Self::iconst(b, 1);
                        let r = b.ins().bxor(v, one);
                        self.define(b, *dst, r)?;
                    }
                    other => {
                        // Statically impossible (the checker rejects
                        // `!` on non-bools); the body span stands in
                        // for the missing operand span.
                        let msg =
                            format!("`!` needs a bool, got {}", Self::static_type_name(&other));
                        let r = self.emit_dead_panic(
                            sess,
                            b,
                            E_MISMATCH,
                            (self.body_span.start as u64, self.body_span.end as u64),
                            &msg,
                        )?;
                        self.define(b, *dst, r)?;
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
                // Hoist argument reads and publish once: every path
                // below may collect.
                let mut argvals = Vec::with_capacity(args.len());
                for a in args {
                    argvals.push(self.operand(b, *a)?);
                }
                self.publish_all(sess, b)?;
                let ret = match self.funcrefs.get(func).copied() {
                    Some(target) => {
                        self.lower_direct_call(sess, b, prog, target, &argvals, span)?
                    }
                    None => self.lower_indirect_call(sess, b, *func, &argvals, span, display)?,
                };
                if let Some(d) = dst {
                    self.define(b, *d, ret)?;
                }
            }
            MInstr::FuncRef { dst, idx } => {
                if prog.fns.get(*idx).is_none() {
                    return self.fail(format!("funcref index {idx} out of range"));
                }
                // Materialize the index (higher-order values read it);
                // direct calls resolve statically at each `Call`.
                self.funcrefs.insert(*dst, *idx);
                let v = Self::iconst(b, *idx as i64);
                self.define(b, *dst, v)?;
            }
            MInstr::CallForeign {
                dst,
                name,
                args,
                span,
            } => {
                // Foreign calls lower to `toto_foreign`, which reuses
                // `ffi::call` for marshaling (range checks, NUL-copies,
                // in-band errno, pool execution in task mode): C
                // scalars ride Toto-view tags, strings cross as
                // handles the shim copies, pointers as `TAG_PTR`.
                let sig = prog.externs.get(name).cloned().ok_or_else(|| SubsetError {
                    func: self.func.clone(),
                    message: format!(
                        "foreign function `{name}` has no checked signature (unreachable: checked code)"
                    ),
                })?;
                // Callbacks need evaluator drain with callable
                // invocation, which generated code cannot do: refuse
                // the call with the workaround, never miscompile it.
                if sig.params.iter().any(|(_, ty)| matches!(ty, Ty::Fn(..))) {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` takes a callback, which the JIT does not support (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // Network calls need the M:N poller (a worker or pool
                // thread must never block on a network wait), and the
                // JIT task runtime has no poller integration: refuse
                // with the workaround, never miscompile or stall in
                // generated code. AOT shares this lowering, so the
                // same refusal covers `toto build` (native net I/O is
                // a recorded follow-up; the executor is the reference).
                if name.starts_with("toto_rt_net_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` is a network wait, which needs the M:N poller (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // Crypto calls lower through the executor (the runtime
                // task executor runs them inline); the JIT task runtime
                // has no byte-array foreign ABI wired, so refuse with
                // the workaround like the net surface, never
                // miscompile. AOT shares this lowering, so the same
                // refusal covers `toto build` (native crypto in AOT is
                // a recorded follow-up; the executor is the reference).
                if name.starts_with("toto_rt_crypto_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` needs the executor crypto runtime (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // Compression lowers through the executor like the
                // other byte-array surfaces (run with `toto run`/`toto
                // test`); AOT-native gzip is a recorded follow-up.
                if name.starts_with("toto_rt_gzip_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` needs the executor gzip runtime (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // TLS lowers through the executor's blocking pool (a
                // handshake/record step blocks on the network); the
                // JIT task runtime has no pool integration, so refuse
                // with the workaround like the net surface, never
                // miscompile or stall in generated code. AOT shares
                // this lowering, so the same refusal covers `toto
                // build` (native TLS in AOT is a recorded follow-up;
                // the executor is the reference).
                if name.starts_with("toto_rt_tls_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` is a blocking TLS wait, which needs the executor pool (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // Context, clock, and logging calls lower through the
                // executor (per-task slots, spawn inheritance,
                // suspension-point deadline checks); the JIT task
                // runtime has no slot integration, so refuse with the
                // workaround like the other Wave 2 surfaces, never
                // miscompile. AOT shares this lowering, so the same
                // refusal covers `toto build` (native context in AOT
                // is a recorded follow-up; the executor is the
                // reference).
                if name.starts_with("toto_rt_ctx_")
                    || name.starts_with("toto_rt_clock_")
                    || name.starts_with("toto_rt_log_")
                {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` needs the executor context runtime (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // Database calls lower through the executor's
                // blocking pool (a checkout or a query blocks); the
                // JIT task runtime has no pool integration, so
                // refuse with the workaround like the other pool
                // surfaces, never miscompile or stall in generated
                // code. AOT shares this lowering, so the same
                // refusal covers `toto build` (native DB in AOT is
                // a recorded follow-up; the executor is the
                // reference).
                if name.starts_with("toto_rt_db_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` is a blocking database wait, which needs the executor pool (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                // LDAP calls lower the same way (a connect/bind
                // round-trip blocks); same refusal, same reason.
                if name.starts_with("toto_rt_ldap_") {
                    return Err(SubsetError {
                        func: self.func.clone(),
                        message: format!(
                            "foreign function `{name}` is a blocking LDAP wait, which needs the executor pool (run with `toto run`/`toto test`)"
                        ),
                    });
                }
                let idx = Self::extern_index(prog, name).ok_or_else(|| SubsetError {
                    func: self.func.clone(),
                    message: format!(
                        "foreign function `{name}` has no runtime index (unreachable: checked code)"
                    ),
                })?;
                // Marshal (tag, word) pairs into a stack slot, exactly
                // like builtins.
                let slot = Self::stack_slot(b, (args.len().max(1) * 16) as u32);
                for (i, a) in args.iter().enumerate() {
                    let ty = self.local_ty(*a)?.clone();
                    let tag = Self::tag_of(&ty).map_err(|e| SubsetError {
                        func: self.func.clone(),
                        message: format!("foreign `{name}` argument {}: {e}", i + 1),
                    })?;
                    let v = self.operand(b, *a)?;
                    let tag_v = Self::iconst(b, tag as i64);
                    Self::stack_store(b, slot, 2 * i, tag_v);
                    Self::stack_store(b, slot, 2 * i + 1, v);
                }
                // Publish before the call (the shim may allocate and
                // collect; frozen safepoint-call rule).
                self.publish_all(sess, b)?;
                let idx_v = Self::iconst(b, idx as i64);
                let n_v = Self::iconst(b, args.len() as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_foreign",
                    &[self.ctx, idx_v, slot, n_v, start_v, end_v],
                )?;
                // Foreign calls push no trace frame on failure (leaf
                // errors, like builtins): silent unwind.
                self.check_silent(sess, b, *span)?;
                if let Some(d) = dst {
                    self.define(b, *d, v)?;
                }
            }
            MInstr::CallBuiltin {
                dst,
                name,
                args,
                span,
            } => {
                // Channel builtins run natively (never dispatched).
                if name == "chan" {
                    return self.lower_builtin_chan(sess, b, *dst, args, span);
                }
                if name == "close" || name == "closed" {
                    return self.lower_builtin_close(sess, b, *dst, name, args, span);
                }
                let id = crate::runtime::builtin_id(name).ok_or_else(|| SubsetError {
                    func: self.func.clone(),
                    message: format!("unknown builtin `{name}` (unreachable: checked code)"),
                })?;
                // Marshal (tag, word) pairs into a stack slot.
                let slot = Self::stack_slot(b, (args.len().max(1) * 16) as u32);
                for (i, a) in args.iter().enumerate() {
                    let ty = self.local_ty(*a)?.clone();
                    let tag = Self::tag_of(&ty).map_err(|e| SubsetError {
                        func: self.func.clone(),
                        message: format!("builtin `{name}` argument {}: {e}", i + 1),
                    })?;
                    let v = self.operand(b, *a)?;
                    let tag_v = Self::iconst(b, tag as i64);
                    Self::stack_store(b, slot, 2 * i, tag_v);
                    Self::stack_store(b, slot, 2 * i + 1, v);
                }
                // Publish before the call: builtins may allocate and
                // collect (frozen safepoint-call rule).
                self.publish_all(sess, b)?;
                let id_v = Self::iconst(b, id as i64);
                let n_v = Self::iconst(b, args.len() as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_builtin",
                    &[self.ctx, id_v, slot, n_v, start_v, end_v],
                )?;
                // Builtins push no trace frame on failure (the
                // executor's leaf errors carry no trace of their own):
                // silent unwind, like every other helper call.
                self.check_silent(sess, b, *span)?;
                if let Some(d) = dst {
                    self.define(b, *d, v)?;
                }
            }
            MInstr::ArrayLit { dst, elems, span } => {
                // Publish first (threshold check inside the allocator),
                // then allocate plus one initializing write per element.
                self.publish_all(sess, b)?;
                let alen = Self::iconst(b, elems.len() as i64);
                let astart = Self::iconst(b, span.start as i64);
                let aend = Self::iconst(b, span.end as i64);
                let arr =
                    self.call_value(sess, b, "toto_alloc_array", &[self.ctx, alen, astart, aend])?;
                self.check_silent(sess, b, *span)?;
                for (i, e) in elems.iter().enumerate() {
                    let v = self.operand(b, *e)?;
                    let ety = self.local_ty(*e)?.clone();
                    let tag = Self::tag_of(&ety).map_err(|e| SubsetError {
                        func: self.func.clone(),
                        message: format!("array element: {e}"),
                    })?;
                    let btag = Self::iconst(b, TAG_HANDLE as i64);
                    let itag = Self::iconst(b, TAG_INT as i64);
                    let idx = Self::iconst(b, i as i64);
                    let vtag = Self::iconst(b, tag as i64);
                    // Index span: the literal span (initializing writes
                    // never fail bounds/type checks).
                    self.call_import(
                        sess,
                        b,
                        "toto_index_write",
                        &[
                            self.ctx, arr, btag, itag, idx, vtag, v, astart, aend, astart, aend,
                        ],
                    )?;
                    self.check_silent(sess, b, *span)?;
                }
                self.define(b, *dst, arr)?;
            }
            MInstr::Index {
                dst,
                base,
                index,
                span,
                index_span,
            } => {
                let bty = self.local_ty(*base)?.clone();
                let ity = self.local_ty(*index)?.clone();
                let btag = Self::tag_of(&bty).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("index base: {e}"),
                })?;
                let itag = Self::tag_of(&ity).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("index operand: {e}"),
                })?;
                // Pure read (no collection inside): no publish needed,
                // but the call is fallible, so check it.
                let bbase = self.operand(b, *base)?;
                let bidx = self.operand(b, *index)?;
                let btag_v = Self::iconst(b, btag as i64);
                let itag_v = Self::iconst(b, itag as i64);
                let bstart = Self::iconst(b, span.start as i64);
                let bend = Self::iconst(b, span.end as i64);
                let istart = Self::iconst(b, index_span.start as i64);
                let iend = Self::iconst(b, index_span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_index_read",
                    &[
                        self.ctx, bbase, btag_v, itag_v, bidx, bstart, bend, istart, iend,
                    ],
                )?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::Len { dst, base, span } => {
                let bty = self.local_ty(*base)?.clone();
                let tag = Self::tag_of(&bty).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("len base: {e}"),
                })?;
                // Pure read (no collection inside): no publish needed,
                // but the call is fallible, so check it.
                let lbase = self.operand(b, *base)?;
                let ltag = Self::iconst(b, tag as i64);
                let lstart = Self::iconst(b, span.start as i64);
                let lend = Self::iconst(b, span.end as i64);
                let v =
                    self.call_value(sess, b, "toto_len", &[self.ctx, lbase, ltag, lstart, lend])?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::StateGet { dst, slot, span } => {
                // Declared-state read (spec amendment, Phase 6): pure
                // read through the helper (no collection inside, so no
                // publish needed — same discipline as `Len`). The word
                // is opaque here; the caller interprets it by the
                // slot's static type. The slot index is a
                // compile-time constant.
                let lslot = Self::iconst(b, *slot as i64);
                let lstart = Self::iconst(b, span.start as i64);
                let lend = Self::iconst(b, span.end as i64);
                let v =
                    self.call_value(sess, b, "toto_state_get", &[self.ctx, lslot, lstart, lend])?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::StateSet { slot, src, span } => {
                // Declared-state write: the value crosses by (tag,
                // word) like every other heap traffic; the helper
                // validates liveness and records loudly on failure.
                // No publish needed (no collection inside); the stored
                // cell is traced at the next collection by the root
                // extension (`gather_roots` + gate collects).
                let tag = Self::tag_of(&self.local_ty(*src)?.clone()).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("state_set: {e}"),
                })?;
                let lslot = Self::iconst(b, *slot as i64);
                let ltag = Self::iconst(b, tag as i64);
                let lsrc = self.operand(b, *src)?;
                let lstart = Self::iconst(b, span.start as i64);
                let lend = Self::iconst(b, span.end as i64);
                let _ = self.call_value(
                    sess,
                    b,
                    "toto_state_set",
                    &[self.ctx, lslot, ltag, lsrc, lstart, lend],
                )?;
                self.check_silent(sess, b, *span)?;
            }
            // Coverage markers vanish under JIT/AOT (Wave 8): they
            // exist only in `lower_for_coverage` output, which the
            // JIT never sees — this arm is belt and braces.
            MInstr::Cover { .. } => {}
            MInstr::Safepoint { kind } => {
                use toto_compiler::mir::SpKind as K;
                self.publish_all(sess, b)?;
                // Task mode joins collections at loop/call/entry
                // points (the rendezvous needs every task to arrive;
                // allocation and suspension safepoints collect inside
                // their helpers instead). Gate mode collects in
                // allocation helpers only.
                if self.mode == CodegenMode::Task && matches!(kind, K::Loop | K::Call | K::Entry) {
                    self.call_import(sess, b, "toto_gc_collect", &[self.ctx])?;
                }
            }
            MInstr::StructLit {
                dst,
                name,
                fields,
                span,
            } => {
                self.publish_all(sess, b)?;
                let (nptr, nlen) = sess.intern_str(b, name);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let obj = self.call_value(
                    sess,
                    b,
                    "toto_alloc_struct",
                    &[self.ctx, nptr, nlen, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                for (fname, l) in fields {
                    let v = self.operand(b, *l)?;
                    let ety = self.local_ty(*l)?.clone();
                    let tag = Self::tag_of(&ety).map_err(|e| SubsetError {
                        func: self.func.clone(),
                        message: format!("struct field `{fname}`: {e}"),
                    })?;
                    let (fptr_v, flen_v) = sess.intern_str(b, fname);
                    let vtag = Self::iconst(b, tag as i64);
                    self.call_import(
                        sess,
                        b,
                        "toto_struct_init",
                        &[self.ctx, obj, fptr_v, flen_v, vtag, v, start_v, end_v],
                    )?;
                    self.check_silent(sess, b, *span)?;
                }
                self.define(b, *dst, obj)?;
            }
            MInstr::Snapshot { dst, base, span } => {
                use cranelift_codegen::ir::condcodes::IntCC;
                // Clone loop: fresh array, then copy element-wise.
                // (No safepoint inside: bounded straight-line work;
                // the allocator collected before inserting.)
                let bobj = self.operand(b, *base)?;
                let btag = Self::iconst(b, TAG_HANDLE as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let len =
                    self.call_value(sess, b, "toto_len", &[self.ctx, bobj, btag, start_v, end_v])?;
                self.check_silent(sess, b, *span)?;
                let ety = match self.local_ty(*base)?.clone() {
                    Ty::Array(t) => *t,
                    other => {
                        return self.fail(format!(
                            "snapshot base has non-array type {other:?} (unreachable)"
                        ));
                    }
                };
                let vtag = Self::tag_of(&ety).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("snapshot element: {e}"),
                })?;
                self.publish_all(sess, b)?;
                let arr = self.call_value(
                    sess,
                    b,
                    "toto_alloc_array",
                    &[self.ctx, len, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                let zero = Self::iconst(b, 0);
                let one = Self::iconst(b, 1);
                let itag = Self::iconst(b, TAG_INT as i64);
                let vtag_v = Self::iconst(b, vtag as i64);
                let head_b = b.create_block();
                let body_b = b.create_block();
                let exit_b = b.create_block();
                b.append_block_param(head_b, I64);
                b.ins().jump(head_b, &[BlockArg::Value(zero)]);
                b.switch_to_block(head_b);
                let i = b.block_params(head_b)[0];
                let c = b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                b.ins().brif(c, body_b, &[], exit_b, &[]);
                b.switch_to_block(body_b);
                b.seal_block(body_b);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_index_read",
                    &[
                        self.ctx, bobj, btag, itag, i, start_v, end_v, start_v, end_v,
                    ],
                )?;
                self.check_silent(sess, b, *span)?;
                self.call_import(
                    sess,
                    b,
                    "toto_index_write",
                    &[
                        self.ctx, arr, btag, itag, i, vtag_v, v, start_v, end_v, start_v, end_v,
                    ],
                )?;
                self.check_silent(sess, b, *span)?;
                let i1 = b.ins().iadd(i, one);
                b.ins().jump(head_b, &[BlockArg::Value(i1)]);
                b.seal_block(head_b);
                b.switch_to_block(exit_b);
                b.seal_block(exit_b);
                self.define(b, *dst, arr)?;
            }
            MInstr::RangeLit { dst, lo, hi, span } => {
                let lo_v = self.operand(b, *lo)?;
                let hi_v = self.operand(b, *hi)?;
                self.publish_all(sess, b)?;
                let v = self.call_value(sess, b, "toto_alloc_range", &[self.ctx, lo_v, hi_v])?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::SplitRange {
                lo,
                hi,
                range,
                span,
            } => {
                let r = self.operand(b, *range)?;
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let lo_v =
                    self.call_value(sess, b, "toto_range_lo", &[self.ctx, r, start_v, end_v])?;
                self.check_silent(sess, b, *span)?;
                let hi_v =
                    self.call_value(sess, b, "toto_range_hi", &[self.ctx, r, start_v, end_v])?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *lo, lo_v)?;
                self.define(b, *hi, hi_v)?;
            }
            MInstr::WrapOk { dst, inner } => {
                let (tag_v, word) = self.tagged_operand(b, *inner)?;
                self.publish_all(sess, b)?;
                let is_ok = Self::iconst(b, 1);
                let (start_v, end_v) = self.body_span_words(b);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_alloc_result",
                    &[self.ctx, is_ok, tag_v, word, start_v, end_v],
                )?;
                self.check_silent(sess, b, self.body_span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::WrapErr { dst, inner } => {
                let (tag_v, word) = self.tagged_operand(b, *inner)?;
                self.publish_all(sess, b)?;
                let is_ok = Self::iconst(b, 0);
                let (start_v, end_v) = self.body_span_words(b);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_alloc_result",
                    &[self.ctx, is_ok, tag_v, word, start_v, end_v],
                )?;
                self.check_silent(sess, b, self.body_span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::WrapSome { dst, inner } => {
                let (tag_v, word) = self.tagged_operand(b, *inner)?;
                self.publish_all(sess, b)?;
                let (start_v, end_v) = self.body_span_words(b);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_alloc_some",
                    &[self.ctx, tag_v, word, start_v, end_v],
                )?;
                self.check_silent(sess, b, self.body_span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::Propagate { dst, operand, span } => {
                use cranelift_codegen::ir::condcodes::IntCC;
                let ty = self.local_ty(*operand)?.clone();
                let v = self.operand(b, *operand)?;
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                match ty {
                    Ty::Result(..) => {
                        let is_ok = self.call_value(
                            sess,
                            b,
                            "toto_result_is_ok",
                            &[self.ctx, v, start_v, end_v],
                        )?;
                        self.check_silent(sess, b, *span)?;
                        let zero = Self::iconst(b, 0);
                        let ok_c = b.ins().icmp(IntCC::NotEqual, is_ok, zero);
                        let ok_b = b.create_block();
                        let err_b = b.create_block();
                        let cont_b = b.create_block();
                        b.ins().brif(ok_c, ok_b, &[], err_b, &[]);
                        b.switch_to_block(err_b);
                        b.seal_block(err_b);
                        self.epilogue(sess, b)?;
                        b.ins().return_(&[v]);
                        b.switch_to_block(ok_b);
                        b.seal_block(ok_b);
                        let inner = self.call_value(
                            sess,
                            b,
                            "toto_wrap_inner",
                            &[self.ctx, v, start_v, end_v],
                        )?;
                        self.check_silent(sess, b, *span)?;
                        self.define(b, *dst, inner)?;
                        b.ins().jump(cont_b, &[]);
                        b.switch_to_block(cont_b);
                        b.seal_block(cont_b);
                    }
                    Ty::Option(_) => {
                        // None is word 0: nonzero unwraps, zero returns.
                        let zero = Self::iconst(b, 0);
                        let some_c = b.ins().icmp(IntCC::NotEqual, v, zero);
                        let some_b = b.create_block();
                        let none_b = b.create_block();
                        let cont_b = b.create_block();
                        b.ins().brif(some_c, some_b, &[], none_b, &[]);
                        b.switch_to_block(none_b);
                        b.seal_block(none_b);
                        self.epilogue(sess, b)?;
                        b.ins().return_(&[zero]);
                        b.switch_to_block(some_b);
                        b.seal_block(some_b);
                        let inner = self.call_value(
                            sess,
                            b,
                            "toto_wrap_inner",
                            &[self.ctx, v, start_v, end_v],
                        )?;
                        self.check_silent(sess, b, *span)?;
                        self.define(b, *dst, inner)?;
                        b.ins().jump(cont_b, &[]);
                        b.switch_to_block(cont_b);
                        b.seal_block(cont_b);
                    }
                    _ => {
                        // Statically impossible (`?` needs a `Result`
                        // or `Option` operand).
                        let r = self.emit_dead_panic(
                            sess,
                            b,
                            E_INTERNAL,
                            (span.start as u64, span.end as u64),
                            "`?` outside a matching Result/Option function (unreachable in checked programs)",
                        )?;
                        self.define(b, *dst, r)?;
                    }
                }
            }
            MInstr::Field {
                dst,
                base,
                name,
                span,
            } => {
                let bobj = self.operand(b, *base)?;
                let (nptr, nlen) = sess.intern_str(b, name);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_struct_get",
                    &[self.ctx, bobj, nptr, nlen, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::StoreField {
                base,
                name,
                value,
                span,
            } => {
                let bobj = self.operand(b, *base)?;
                let (vtag, vword) = self.tagged_operand(b, *value)?;
                let (nptr, nlen) = sess.intern_str(b, name);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                self.call_import(
                    sess,
                    b,
                    "toto_struct_set",
                    &[self.ctx, bobj, nptr, nlen, vtag, vword, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
            }
            MInstr::StoreIndex {
                base,
                index,
                value,
                span,
                index_span,
            } => {
                let bobj = self.operand(b, *base)?;
                let bty = self.local_ty(*base)?.clone();
                let ity = self.local_ty(*index)?.clone();
                let btag = Self::tag_of(&bty).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("store base: {e}"),
                })?;
                let itag = Self::tag_of(&ity).map_err(|e| SubsetError {
                    func: self.func.clone(),
                    message: format!("store index: {e}"),
                })?;
                let idx = self.operand(b, *index)?;
                let (vtag, vword) = self.tagged_operand(b, *value)?;
                let btag_v = Self::iconst(b, btag as i64);
                let itag_v = Self::iconst(b, itag as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let istart = Self::iconst(b, index_span.start as i64);
                let iend = Self::iconst(b, index_span.end as i64);
                self.call_import(
                    sess,
                    b,
                    "toto_index_write",
                    &[
                        self.ctx, bobj, btag_v, itag_v, idx, vtag, vword, start_v, end_v, istart,
                        iend,
                    ],
                )?;
                self.check_silent(sess, b, *span)?;
            }
            MInstr::SatIncr { dst, operand } => {
                use cranelift_codegen::ir::condcodes::IntCC;
                let v = self.operand(b, *operand)?;
                let max = Self::iconst(b, i64::MAX);
                let is_max = b.ins().icmp(IntCC::Equal, v, max);
                let one = Self::iconst(b, 1);
                let r = Self::if_else(
                    sess,
                    b,
                    is_max,
                    |_, b| Self::iconst(b, i64::MAX),
                    |_, b| b.ins().iadd(v, one),
                );
                self.define(b, *dst, r)?;
            }
            MInstr::MakeClosure {
                dst,
                body,
                captures,
                span,
            } => {
                // Assign a body id and collect the body for later
                // compilation (both modes lower closures).
                let id = sess.alloc_body(BodyKind::Closure, Arc::clone(body));
                let slot = Self::stack_slot(b, (captures.len().max(1) * 16) as u32);
                for (i, c) in captures.iter().enumerate() {
                    let (tag_v, word) = self.tagged_operand(b, *c)?;
                    Self::stack_store(b, slot, 2 * i, tag_v);
                    Self::stack_store(b, slot, 2 * i + 1, word);
                }
                self.publish_all(sess, b)?;
                let id_v = Self::iconst(b, id as i64);
                let n_v = Self::iconst(b, captures.len() as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_alloc_closure",
                    &[self.ctx, id_v, slot, n_v, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::ChanNew {
                dst,
                elem,
                cap,
                span,
            } => {
                let v = self.chan_new_emit(sess, b, elem, *cap, span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::Send { chan, value, span } => {
                if self.mode == CodegenMode::Gate {
                    return self.fail(String::from(
                        "channels need a task session (concurrency is task-only)",
                    ));
                }
                let chan_w = self.operand(b, *chan)?;
                let (vtag, vword) = self.tagged_operand(b, *value)?;
                // Roots ride the preceding `Park` publication; the
                // suspender collects before parking.
                let op = Self::iconst(b, crate::task::YIELD_SEND as i64);
                let z = Self::iconst(b, 0);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                self.call_import(
                    sess,
                    b,
                    "toto_yield",
                    &[self.ctx, op, chan_w, vtag, vword, z, z, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
            }
            MInstr::Recv { dst, chan, span } => {
                if self.mode == CodegenMode::Gate {
                    return self.fail(String::from(
                        "channels need a task session (concurrency is task-only)",
                    ));
                }
                let chan_w = self.operand(b, *chan)?;
                let op = Self::iconst(b, crate::task::YIELD_RECV as i64);
                let z = Self::iconst(b, 0);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_yield",
                    &[self.ctx, op, chan_w, z, z, z, z, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
            }
            MInstr::Spawn {
                body,
                captures,
                span,
            } => {
                if self.mode == CodegenMode::Gate {
                    return self.fail(String::from(
                        "spawn needs a task session (concurrency is task-only)",
                    ));
                }
                let id = sess.alloc_body(BodyKind::Go, Arc::clone(body));
                let slot = Self::stack_slot(b, (captures.len().max(1) * 16) as u32);
                for (i, c) in captures.iter().enumerate() {
                    let (tag_v, word) = self.tagged_operand(b, *c)?;
                    Self::stack_store(b, slot, 2 * i, tag_v);
                    Self::stack_store(b, slot, 2 * i + 1, word);
                }
                self.publish_all(sess, b)?;
                let id_v = Self::iconst(b, id as i64);
                let n_v = Self::iconst(b, captures.len() as i64);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                self.call_import(
                    sess,
                    b,
                    "toto_spawn",
                    &[self.ctx, id_v, slot, n_v, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
            }
            MInstr::Select {
                dst,
                cases,
                has_default,
                span,
            } => {
                if self.mode == CodegenMode::Gate {
                    return self.fail(String::from(
                        "select needs a task session (concurrency is task-only)",
                    ));
                }
                // Marshal 6-word case records + an out word.
                let slot = Self::stack_slot(b, (cases.len().max(1) * 48) as u32);
                for (i, case) in cases.iter().enumerate() {
                    let base = 6 * i;
                    match case {
                        MSelectCase::Recv {
                            chan, span: cspan, ..
                        } => {
                            let chan_w = self.operand(b, *chan)?;
                            let z = Self::iconst(b, 0);
                            let cs = Self::iconst(b, cspan.start as i64);
                            let ce = Self::iconst(b, cspan.end as i64);
                            Self::stack_store(b, slot, base, z);
                            Self::stack_store(b, slot, base + 1, chan_w);
                            Self::stack_store(b, slot, base + 2, z);
                            Self::stack_store(b, slot, base + 3, z);
                            Self::stack_store(b, slot, base + 4, cs);
                            Self::stack_store(b, slot, base + 5, ce);
                        }
                        MSelectCase::Send {
                            chan,
                            value,
                            span: cspan,
                        } => {
                            let chan_w = self.operand(b, *chan)?;
                            let (vtag, vword) = self.tagged_operand(b, *value)?;
                            let kind_v = Self::iconst(b, 1);
                            let cs = Self::iconst(b, cspan.start as i64);
                            let ce = Self::iconst(b, cspan.end as i64);
                            Self::stack_store(b, slot, base, kind_v);
                            Self::stack_store(b, slot, base + 1, chan_w);
                            Self::stack_store(b, slot, base + 2, vtag);
                            Self::stack_store(b, slot, base + 3, vword);
                            Self::stack_store(b, slot, base + 4, cs);
                            Self::stack_store(b, slot, base + 5, ce);
                        }
                    }
                }
                let out_slot = Self::stack_slot(b, 8);
                self.publish_all(sess, b)?;
                let op = Self::iconst(b, crate::task::YIELD_SELECT as i64);
                let n_v = Self::iconst(b, cases.len() as i64);
                let hd_v = Self::iconst(b, i64::from(*has_default));
                let z = Self::iconst(b, 0);
                let start_v = Self::iconst(b, span.start as i64);
                let end_v = Self::iconst(b, span.end as i64);
                let v = self.call_value(
                    sess,
                    b,
                    "toto_yield",
                    &[self.ctx, op, slot, n_v, hd_v, out_slot, z, start_v, end_v],
                )?;
                self.check_silent(sess, b, *span)?;
                self.define(b, *dst, v)?;
                // Bind slots read the out word: only the selected
                // case's arm uses its bind, and the out word is
                // always a live value (or 0), so unused binds
                // publish safely.
                let out_v = Self::stack_load(b, out_slot, 0);
                for case in cases {
                    if let MSelectCase::Recv { bind: Some(l), .. } = case {
                        self.define(b, *l, out_v)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Body-span words for spanless instructions (wrappers carry no
    /// span; their failures are unreachable backstops).
    fn body_span_words(&self, b: &mut FunctionBuilder<'_>) -> (CVal, CVal) {
        (
            Self::iconst(b, self.body_span.start as i64),
            Self::iconst(b, self.body_span.end as i64),
        )
    }

    /// Shared channel creation: interns the element type, publishes,
    /// and calls `toto_chan_make` (task-only).
    fn chan_new_emit(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        elem: &Ty,
        cap: Option<usize>,
        span: &Span,
    ) -> Result<CVal, SubsetError> {
        if self.mode == CodegenMode::Gate {
            return self.fail(String::from(
                "channels need a task session (concurrency is task-only)",
            ));
        }
        let id = sess.intern_chan_type(elem.clone());
        self.publish_all(sess, b)?;
        let (has_cap, cap_w) = match cap {
            Some(l) => {
                let w = self.operand(b, l)?;
                (Self::iconst(b, 1), w)
            }
            None => (Self::iconst(b, 0), Self::iconst(b, 0)),
        };
        let ty_v = Self::iconst(b, id as i64);
        let start_v = Self::iconst(b, span.start as i64);
        let end_v = Self::iconst(b, span.end as i64);
        let v = self.call_value(
            sess,
            b,
            "toto_chan_make",
            &[self.ctx, has_cap, cap_w, ty_v, start_v, end_v],
        )?;
        self.check_silent(sess, b, *span)?;
        Ok(v)
    }

    /// Defensive `chan` builtin (normally `ChanNew`): capacity from
    /// the argument list, element type from the destination.
    fn lower_builtin_chan(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        dst: Option<usize>,
        args: &[usize],
        span: &Span,
    ) -> Result<(), SubsetError> {
        let d = dst.ok_or_else(|| SubsetError {
            func: self.func.clone(),
            message: String::from("chan without a destination (unreachable: checked code)"),
        })?;
        let elem = match self.local_ty(d)?.clone() {
            Ty::Chan(t) => *t,
            other => {
                return self.fail(format!(
                    "chan destination has type {other:?} (unreachable: checked code)"
                ));
            }
        };
        let cap = match args.len() {
            0 => None,
            1 => Some(args[0]),
            _ => {
                return self.fail(format!(
                    "chan takes 0 or 1 arguments, got {} (unreachable: checked code)",
                    args.len()
                ));
            }
        };
        let v = self.chan_new_emit(sess, b, &elem, cap, span)?;
        self.define(b, d, v)?;
        Ok(())
    }

    /// Native `close`/`closed` (task-only): scheduler round-trips.
    fn lower_builtin_close(
        &self,
        sess: &mut impl Session,
        b: &mut FunctionBuilder<'_>,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: &Span,
    ) -> Result<(), SubsetError> {
        if self.mode == CodegenMode::Gate {
            return self.fail(format!(
                "builtin `{name}` needs a task session (concurrency is task-only)"
            ));
        }
        if args.len() != 1 {
            return self.fail(format!(
                "builtin `{name}` takes 1 argument, got {} (unreachable: checked code)",
                args.len()
            ));
        }
        self.publish_all(sess, b)?;
        let chan = self.operand(b, args[0])?;
        let op = if name == "close" {
            crate::task::YIELD_CLOSE
        } else {
            crate::task::YIELD_CLOSED
        };
        let op_v = Self::iconst(b, op as i64);
        let z = Self::iconst(b, 0);
        let start_v = Self::iconst(b, span.start as i64);
        let end_v = Self::iconst(b, span.end as i64);
        // (op, chan, 0, 0, 0, 0, span): only the channel word matters.
        let v = self.call_value(
            sess,
            b,
            "toto_yield",
            &[self.ctx, op_v, chan, z, z, z, z, start_v, end_v],
        )?;
        // `close`/`closed` push no trace frame on failure (builtins
        // carry no trace of their own in the reference executor):
        // silent unwind, like every other helper call.
        self.check_silent(sess, b, *span)?;
        if let Some(d) = dst {
            self.define(b, d, v)?;
        }
        Ok(())
    }
}
