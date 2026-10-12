//! The interpreter: executes Dalvik bytecode against a small object heap,
//! plus a handful of built-in shims for core library methods.

use crate::dex::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_DEPTH: usize = 2000;
const NO_INDEX: u32 = 0xffff_ffff;

/// How many saved cursors sit at the front of a Composer's slot vector. They
/// are a run of Ints and slots hold references, so the two never collide.
fn stack_depth(stack: &[Value]) -> usize {
    stack.iter().take_while(|v| matches!(v, Value::Int(_))).count()
}

/// Instructions executed so far, across every call frame.
static STEPS: AtomicU64 = AtomicU64::new(0);

/// Set `APKNATIVE_MAX_STEPS` to bound execution.
///
/// Any app with a frame loop -- Compose's recomposer, anything animating --
/// never returns on its own, because there is no clock to advance it. That
/// makes the process unkillable by any means short of SIGKILL, which is a
/// bad way to end a run. The budget stops execution deterministically and says
/// so, which is also what makes a hung app debuggable rather than mysterious.
fn max_steps() -> u64 {
    static M: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("APKNATIVE_MAX_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50_000_000)
    })
}

/// Set `APKNATIVE_TRACE=1` to dump every executed instruction.
fn tracing() -> bool {
    use std::sync::OnceLock;
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("APKNATIVE_TRACE").is_some())
}

/// Length of an instruction in 16-bit code units, from the Dalvik format
/// table. Getting this wrong desynchronises the whole instruction stream, so
/// it lives in one table instead of being tracked by each opcode arm.
pub(crate) fn insn_width(op: u8) -> usize {
    match op {
        0x01 | 0x04 | 0x07 => 1,       // move*
        0x02 | 0x05 | 0x08 => 2,       // move*/from16
        0x03 | 0x06 | 0x09 => 3,       // move*/16
        0x0a..=0x12 => 1,              // move-result*, return*, const/4
        0x13 | 0x15 | 0x16 | 0x19 => 2, // const/16, const/high16, const-wide/16, high16
        0x14 | 0x17 => 3,              // const, const-wide/32
        0x18 => 5,                     // const-wide
        0x1a | 0x1c => 2,              // const-string, const-class
        0x1b => 3,                     // const-string/jumbo
        0x1d | 0x1e => 1,              // monitor-enter/exit
        0x1f => 2,                     // check-cast
        0x20 => 2,                     // instance-of (22c)
        0x26 => 3,                     // fill-array-data (31t)
        0x21 => 1,                     // array-length
        0x22 => 2,                     // new-instance
        0x23 => 2,                     // new-array (22c)
        0x24 | 0x25 => 3,              // filled-new-array[/range]
        0x27 | 0x28 => 1,              // throw, goto
        0x29 => 2,                     // goto/16
        0x2a..=0x2c => 3,              // goto/32, switch payloads
        0x2d..=0x31 => 2,              // cmp*
        0x32..=0x3d => 2,              // if-test, if-testz
        0x44..=0x51 => 2,              // aget/aput
        0x52..=0x5f => 2,              // iget/iput (22c)
        0x60..=0x6d => 2,              // sget/sput
        0x6e..=0x72 => 3,              // invoke
        0x74..=0x78 => 3,              // invoke/range
        0x7b..=0x8f => 1,              // unop
        0x90..=0xaf => 2,              // binop
        0xb0..=0xcf => 1,              // binop/2addr
        0xd0..=0xd7 => 2,              // binop/lit16
        0xd8..=0xe2 => 2,              // binop/lit8
        0xfa | 0xfb => 4,              // invoke-polymorphic[/range]
        0xfc | 0xfd => 3,              // invoke-custom[/range]
        0xfe | 0xff => 2,              // const-method-handle, const-class
        _ => 1,                        // nop and anything unimplemented
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Void,
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Ref(u32), // 0 == null
}

impl Value {
    pub fn as_int(&self) -> i32 {
        match self {
            Value::Int(i) => *i,
            Value::Long(l) => *l as i32,
            Value::Float(f) => *f as i32,
            Value::Double(d) => *d as i32,
            Value::Ref(_) => 0,
            Value::Void => 0,
        }
    }
    pub fn as_long(&self) -> i64 {
        match self {
            Value::Int(i) => *i as i64,
            Value::Long(l) => *l,
            Value::Float(f) => *f as i64,
            Value::Double(d) => *d as i64,
            _ => 0,
        }
    }
    pub fn as_ref(&self) -> u32 {
        match self {
            Value::Ref(r) => *r,
            _ => 0,
        }
    }
}

pub struct Object {
    /// Which DEX `class_idx` is an index into. A type index means nothing
    /// outside its own file, so resolving a method on an object needs both
    /// halves -- and the object was allocated while some *other* method was
    /// running, which is not necessarily the one calling it now.
    pub dex: usize,
    pub class_idx: TypeIdx,
    pub fields: HashMap<FieldIdx, Value>,
    pub string: Option<String>,
    pub elems: Option<Vec<Value>>,
    /// For `java.lang.Class` instances: the type this Class represents.
    pub represents: Option<TypeIdx>,
    /// Single-value slot for boxed primitives and atomics.
    pub value: Option<Value>,
}

pub struct Runtime {
    /// Every DEX in the APK. A class hierarchy routinely spans files — in a
    /// real app the Activity lives in `classes2.dex` while its superclass is
    /// in `classes.dex` — so method resolution has to span all of them.
    pub dexes: Vec<Dex>,
    /// Index of the DEX the running method belongs to. Method indices and type
    /// indices are only meaningful relative to their own file.
    cur: usize,
    pub heap: Vec<Object>,
    pub statics: HashMap<FieldIdx, Value>,
    pub system_out: u32,
    /// System services are singletons per process: an app routinely asks for
    /// the same service twice and compares the results. Keyed by name so each
    /// service gets one stable object.
    pub services: HashMap<String, u32>,
    /// Compose's `Empty` sentinel. A composable distinguishes "nothing
    /// remembered yet" from any real value by comparing the slot against
    /// this, so it has to be one object: a fresh one per call never compares
    /// equal to itself and the value is never created.
    empty: u32,
}

impl Runtime {
    pub fn new(dexes: Vec<Dex>) -> Runtime {
        assert!(!dexes.is_empty(), "runtime needs at least one DEX");
        Runtime {
            dexes,
            cur: 0,
            heap: Vec::new(),
            statics: HashMap::new(),
            system_out: 0,
            services: HashMap::new(),
            empty: 0,
        }
    }

    /// The first DEX file, for callers that just need to scan the app.
    pub fn primary(&self) -> &Dex {
        &self.dexes[0]
    }

    /// The DEX the currently-executing method belongs to.
    fn d(&self) -> &Dex {
        &self.dexes[self.cur]
    }

    /// Find a class by descriptor across every DEX file.
    fn resolve_class_any(&self, descriptor: &str) -> Option<(usize, TypeIdx)> {
        for (i, dex) in self.dexes.iter().enumerate() {
            if let Some(pos) = dex
                .types
                .iter()
                .position(|&s| dex.string(s) == descriptor)
            {
                return Some((i, pos as TypeIdx));
            }
        }
        None
    }

    // ---- heap helpers ----------------------------------------------------

    pub fn alloc(&mut self, class_idx: TypeIdx) -> u32 {
        self.heap.push(Object {
            dex: self.cur,
            class_idx,
            fields: HashMap::new(),
            string: None,
            elems: None,
            represents: None,
            value: None,
        });
        (self.heap.len() - 1) as u32
    }

    fn new_string(&mut self, s: &str) -> u32 {
        let ci = self.type_index_for("Ljava/lang/String;");
        let idx = self.alloc(ci);
        self.heap[idx as usize].string = Some(s.to_string());
        idx
    }

    fn new_array(&mut self, elem_class: TypeIdx, len: usize) -> u32 {
        let idx = self.alloc(elem_class);
        self.heap[idx as usize].elems = Some(vec![Value::Int(0); len]);
        idx
    }

    /// Allocate a collection whose payload lives in `elems`.
    fn new_holder(&mut self, descriptor: &str) -> u32 {
        let t = self.type_index_for(descriptor);
        let idx = self.alloc(t);
        self.heap[idx as usize].elems = Some(Vec::new());
        idx
    }

    /// Invoke the `@Composable` content lambda that setContent was given.
    ///
    /// The lambda is a `Function2<Composer, Int, Unit>`. Rather than match on
    /// argument position -- which differs between `setContent` and its
    /// `$default` synthetic -- the receiver is located by its class, since
    /// only one argument is a Function2.
    fn compose_content(&mut self, args: &[Value]) -> Option<Value> {
        // The generated implementation declares a concrete
        // invoke(Composer, int) alongside an erased bridge, so call the
        // concrete one and skip the bridge's casts.
        const SIG: &str = "(Landroidx/compose/runtime/Composer;I)Ljava/lang/Object;";

        // Which argument is the content lambda is decided structurally, not
        // by position or name: in a real app it is a ComposableLambda, whose
        // class name says nothing about being a Function2. The one argument
        // that has an invoke(Composer, Int, Object) is it.
        let mut target: Option<(usize, TypeIdx, Value)> = None;
        for v in args.iter().skip(1) {
            let r = v.as_ref();
            if r == 0 {
                continue;
            }
            let Some(ci) = self.heap.get(r as usize).map(|o| o.class_idx) else {
                continue;
            };
            if ci == NO_INDEX {
                continue;
            }
            // A class index is only meaningful inside the DEX it came from,
            // and we do not know which file the object was allocated in, so
            // search them all. lookup validates the descriptor, so a wrong
            // file simply does not match.
            for dexid in 0..self.dexes.len() {
                if self.lookup(dexid, ci, "invoke", Some(SIG)).is_some() {
                    target = Some((dexid, ci, v.clone()));
                    break;
                }
            }
            if target.is_some() {
                break;
            }
        }
        let Some((dexid, ti, recv)) = target else {
            return Some(Value::Void);
        };

        let composer = self.new_holder("Landroidx/compose/runtime/Composer;");
        let saved = self.cur;
        self.cur = dexid;

        // A composable that throws is a real result: report it rather than
        // silently claiming the UI composed.
        let r = match self.lookup(self.cur, ti, "invoke", Some(SIG)) {
            Some((d, enc, m)) if enc.code_off != 0 => self
                // invoke is an instance method: receiver, Composer, changed.
                .exec(d, m, vec![recv, Value::Ref(composer), Value::Int(0)], 0)
                .map(|_| Value::Void),
            _ => Err("content lambda has no invoke".into()),
        };
        self.cur = saved;

        if let Err(e) = &r {
            eprintln!("apknative: composable failed: {e}");
        }
        Some(r.unwrap_or(Value::Void))
    }

    /// The class the Compose compiler generates for a composable lambda.
    /// Found by scanning descriptors rather than hardcoded, so an app that
    /// ships a differently-named implementation still works.
    /// The class the Compose compiler generates for a composable lambda, and
    /// the DEX holding it.
    ///
    /// Both halves are needed together: a type index and a field index are only
    /// meaningful inside their own file, and this class lands in whichever DEX
    /// Compose shipped in, not the one the caller is running from.
    fn composable_lambda_class(&mut self) -> (usize, TypeIdx) {
        for (n, dex) in self.dexes.iter().enumerate() {
            if let Some(i) = dex
                .types
                .iter()
                .position(|&s| dex.string(s).ends_with("/ComposableLambdaImpl;"))
            {
                return (n, i as TypeIdx);
            }
        }
        (self.cur, self.type_index_for(
            "Landroidx/compose/runtime/internal/ComposableLambdaImpl;",
        ))
    }

    /// Index of a field declared directly on a class, searched in one specific
    /// DEX.
    fn field_index_in(&self, dex_id: usize, class: TypeIdx, name: &str) -> Option<FieldIdx> {
        let dex = self.dexes.get(dex_id)?;
        dex.fields
            .iter()
            .position(|f| f.class as u32 == class && dex.string(f.name) == name)
            .map(|i| i as FieldIdx)
    }

    /// Compose's slot table. A `remember` reads the current slot and writes
    /// back the new value, so a Composer that answers every read with null can
    /// never remember anything -- and every value a composable depends on,
    /// including the ViewModelStoreOwner the Activity is supposed to provide,
    /// comes back null.
    ///
    /// Groups nest, so entering one saves the cursor and leaving restores it.
    fn slot_push(&mut self, c: u32) {
        let idx = self.ensure_cell(c, "Landroidx/compose/runtime/Composer;");
        let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0)).as_int();
        let mut stack = self.heap[idx as usize].elems.take().unwrap_or_default();
        // The slot values and the saved cursors share the vector, with the
        // cursors on the front.
        stack.insert(0, Value::Int(cur));
        self.heap[idx as usize].elems = Some(stack);
    }

    fn slot_pop(&mut self, c: u32) {
        let Some(o) = self.heap.get(c as usize) else { return };
        let Some(mut stack) = o.elems.clone() else { return };
        if stack.is_empty() {
            return;
        }
        let cur = stack.remove(0).as_int();
        self.heap[c as usize].elems = Some(stack);
        self.heap[c as usize].value = Some(Value::Int(cur));
    }

    /// Current slot's value without moving on.
    fn slot_peek(&mut self, c: u32) -> Value {
        let idx = self.ensure_cell(c, "Landroidx/compose/runtime/Composer;");
        let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0)).as_int();
        let stack = self.heap[idx as usize].elems.clone().unwrap_or_default();
        // Slot values live past the saved cursors.
        stack
            .get(cur as usize + stack_depth(&stack))
            .cloned()
            .unwrap_or(Value::Ref(self.empty))
    }

    /// Write the current slot and move to the next one.
    fn slot_write(&mut self, c: u32, v: Value) {
        let idx = self.ensure_cell(c, "Landroidx/compose/runtime/Composer;");
        let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0)).as_int();
        let mut stack = self.heap[idx as usize].elems.take().unwrap_or_default();
        let depth = stack_depth(&stack);
        while stack.len() <= depth + cur as usize {
            stack.push(Value::Ref(0));
        }
        stack[depth + cur as usize] = v;
        self.heap[idx as usize].elems = Some(stack);
        self.heap[idx as usize].value = Some(Value::Int(cur + 1));
    }

    /// The process-wide Application object, created on first use.
    fn application(&mut self) -> Value {
        if let Some(i) = self.services.get("__application") {
            return Value::Ref(*i);
        }
        let i = self.new_holder("Landroid/app/Application;");
        self.services.insert("__application".into(), i);
        Value::Ref(i)
    }

    /// A named system service. One object per name, process-wide.
    fn service(&mut self, descriptor: &str) -> Value {
        let key = format!("svc:{descriptor}");
        if let Some(i) = self.services.get(&key) {
            return Value::Ref(*i);
        }
        let i = self.new_holder(descriptor);
        self.services.insert(key, i);
        Value::Ref(i)
    }

    /// `getSystemService` takes a *name* like "layout_inflater" but callers
    /// then call methods on it, so it needs a class descriptor.
    fn service_descriptor(&mut self, name: &str) -> u32 {
        match name {
            "layout_inflater" => self.service("Landroid/view/LayoutInflater;").as_ref(),
            "window" => self.service("Landroid/view/WindowManager;").as_ref(),
            "activity_manager" | "window_manager" => {
                self.service("Landroid/app/ActivityManager;").as_ref()
            }
            "notification" => self.service("Landroid/app/NotificationManager;").as_ref(),
            "uimode" => self.service("Landroid/app/UiModeManager;").as_ref(),
            "accessibility" => self.service("Landroid/accessibilityservice/AccessibilityService;").as_ref(),
            "" => 0,
            other => self.service(&format!("Landroid/system/{other};")).as_ref(),
        }
    }

    /// Allocate a mutable single-value cell (boxed values, atomics).
    fn new_cell(&mut self, descriptor: &str, initial: Value) -> u32 {
        let t = self.type_index_for(descriptor);
        let idx = self.alloc(t);
        self.heap[idx as usize].elems = Some(Vec::new());
        self.heap[idx as usize].value = Some(initial);
        idx
    }

    pub fn new_class_object(&mut self, type_idx: TypeIdx) -> u32 {
        let cls_idx = self.type_index_for("Ljava/lang/Class;");
        let idx = self.alloc(cls_idx);
        self.heap[idx as usize].represents = Some(type_idx);
        idx
    }

    fn string_text(&self, v: &Value) -> Option<&str> {
        match v {
            // A reference the heap doesn't actually contain means the code
            // dereferenced something we never allocated (e.g. a stubbed-out
            // framework call). Treat it as absent rather than panicking.
            Value::Ref(r) if *r != 0 && (*r as usize) < self.heap.len() => {
                self.heap[*r as usize].string.as_deref()
            }
            _ => None,
        }
    }

    pub fn type_index_for(&self, descriptor: &str) -> TypeIdx {
        self.d()
            .types
            .iter()
            .position(|&s| self.d().string(s) == descriptor)
            .map(|i| i as TypeIdx)
            .unwrap_or(NO_INDEX)
    }

    fn find_field(&self, class_desc: &str, name: &str) -> Option<FieldIdx> {
        self.d()
            .fields
            .iter()
            .position(|f| {
                self.d().type_name(f.class as TypeIdx) == class_desc
                    && self.d().string(f.name) == name
            })
            .map(|i| i as FieldIdx)
    }

    /// Allocate the shimmed core objects and bind the static fields that point
    /// at them, so `getstatic System.out` resolves to a real object.
    fn boot(&mut self) {
        // Must exist before any Composer does: a fresh slot reads as Empty,
        // and Empty has to be the same object every time.
        if self.empty == 0 {
            self.empty = self.new_holder("Ljava/lang/Object;");
        }
        self.system_out = self.alloc(self.type_index_for("Ljava/io/PrintStream;"));
        let out = Value::Ref(self.system_out);
        if let Some(f) = self.find_field("Ljava/lang/System;", "out") {
            self.statics.insert(f, out.clone());
        }
        if let Some(f) = self.find_field("Ljava/lang/System;", "err") {
            self.statics.insert(f, out);
        }
    }

    // ---- entry point -----------------------------------------------------

    /// Find a `public static void main(String[])` to run.
    pub fn find_main(&self) -> Option<(TypeIdx, MethodIdx)> {
        for class in &self.d().classes {
            let data = match self.d().class_data(class.class_data_off) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for em in &data.direct_methods {
                let mid = &self.d().methods[em.method_idx as usize];
                if self.d().string(mid.name) != "main" {
                    continue;
                }
                if em.access_flags & ACC_STATIC == 0 {
                    continue;
                }
                if let Some(p) = self.d().proto(mid.proto) {
                    let ret_void = self.d().type_name(p.ret) == "V";
                    let one_arg = p.params.len() == 1
                        && self.d().type_name(p.params[0]) == "[Ljava/lang/String;";
                    if ret_void && one_arg {
                        return Some((class.class_idx, em.method_idx));
                    }
                }
            }
        }
        None
    }

    pub fn run_main(&mut self) -> Result<(), String> {
        let (class_idx, method_idx) =
            self.find_main().ok_or("no `main(String[])` found in this APK's DEX")?;

        // Boot the shimmed core classes so instance/static reads resolve.
        self.boot();
        let args_ref = self.new_array(self.type_index_for("Ljava/lang/String;"), 0);
        let argv = self.new_array(self.type_index_for("Ljava/lang/String;"), 1);
        self.heap[argv as usize].elems.as_mut().unwrap()[0] = Value::Ref(args_ref);

        println!("apknative: running {}", self.d().class_name(class_idx));
        println!("--- program output ---");
        self.exec(self.cur, method_idx, vec![Value::Ref(argv)], 0)?;
        println!("--- end ---");
        Ok(())
    }

    /// Start an app the way Android does: instantiate the Activity class and call
/// its `onCreate(Bundle)`. Unlike a `main`, this exercises the real framework
/// entry path.
pub fn run_activity(&mut self, class_name: &str) -> Result<(), String> {
    self.boot();
        // The Activity may live in any DEX file, and so may its superclasses,
        // so resolve across all of them and make that file the current one.
        let (dex_id, type_idx) = self
            .resolve_class_any(&Self::descriptor(class_name))
            .ok_or_else(|| format!("class {class_name} is not defined in any DEX file"))?;
        let saved = self.cur;
        self.cur = dex_id;

    println!("apknative: instantiating {class_name}");
    println!("--- program output ---");

    // Allocate the Activity instance, then run its constructor and onCreate.
    let obj = self.alloc(type_idx);

    // Run the superclass constructor first, as the runtime would.
    if let Some((d, enc, m)) = self.lookup(self.cur, type_idx, "<init>", None) {
        if enc.code_off != 0 {
            let ins = self
                .d()
                .proto(self.d().methods[m as usize].proto)
                .map(|p| p.params.len() + 1) // + receiver
                .unwrap_or(1);
            // Slot 0 is `this`; remaining arguments are all null for now.
            let mut args: Vec<Value> = vec![Value::Void; ins];
            args[0] = Value::Ref(obj);
            self.exec(d, m, args, 0)?;
        }
    }

    // Android drives an Activity through a fixed lifecycle. onCreate is the
    // only one with a body we can execute; the rest are stubbed overrides, but
    // calling them keeps the app's own onStart/onResume logic running.
    for phase in ["onCreate", "onStart", "onResume"] {
        match self.lookup(self.cur, type_idx, phase, None) {
        Some((d, enc, m)) if enc.code_off != 0 => {
            let params = self
                .d()
                .proto(self.d().methods[m as usize].proto)
                .map(|p| p.params.len() + 1) // + receiver
                .unwrap_or(1);
            let mut args: Vec<Value> = vec![Value::Void; params];
            args[0] = Value::Ref(obj);
            self.exec(d, m, args, 0)?;
        }
        _ => {}
        }
    }
    self.cur = saved;
    println!("--- end ---");
    Ok(())
}

    /// Invoke an arbitrary static method directly (`--entry`).
    pub fn run_entry(&mut self, method_idx: MethodIdx) -> Result<(), String> {
        self.boot();
        let class_idx = self.class_of_method(method_idx);
        let mname = self.d().string(self.d().methods[method_idx as usize].name).to_string();

        println!(
            "apknative: running {}.{}()",
            self.d().class_name(class_idx),
            mname
        );
        println!("--- program output ---");

        // M1 doesn't marshal real command-line arguments; hand each parameter
        // a type-appropriate empty value so the callee still gets a frame.
        let argc = self
            .d()
            .proto(self.d().methods[method_idx as usize].proto)
            .map(|p| p.params.len())
            .unwrap_or(0);
        let args: Vec<Value> = (0..argc).map(|_| Value::Ref(0)).collect();
        self.exec(self.cur, method_idx, args, 0)?;
        println!("--- end ---");
        Ok(())
    }

    // ---- method resolution ----------------------------------------------


    /// Walk the class then its superclasses looking for `name`.
    ///
    /// When `sig` is given the method must also match it exactly. Kotlin emits
    /// overloads that differ only in primitive types — `coerceAtLeast(I, I)`
    /// and `coerceAtLeast(D, D)` both take two parameters — so arity alone
    /// picks the wrong one.
    fn lookup(
        &self,
        dex_id: usize,
        class_idx: TypeIdx,
        name: &str,
        sig: Option<&str>,
    ) -> Option<(usize, EncodedMethod, MethodIdx)> {
        let mut cur_dex = dex_id;
        let mut cur = class_idx;
        // Remembered across the whole walk: a name-only match is a last resort,
        // and must not win over an exact match found on a superclass.
        let mut fallback: Option<(usize, EncodedMethod, MethodIdx)> = None;
        // Guard against malformed or cyclic hierarchies.
        for _ in 0..64 {
            if cur == NO_INDEX || cur as usize >= self.dexes[cur_dex].types.len() {
                return fallback;
            }
            let dex = &self.dexes[cur_dex];
            let cd = match dex.class_def(cur) {
                Some(cd) => cd,
                None => return fallback,
            };

            if let Ok(data) = dex.class_data(cd.class_data_off) {
                for em in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
                    let mid = &dex.methods[em.method_idx as usize];
                    if dex.string(mid.name) != name {
                        continue;
                    }
                    match sig {
                        Some(s) => {
                            if self.sig_in(mid) == s {
                                return Some((cur_dex, em.clone(), em.method_idx));
                            }
                            if fallback.is_none() {
                                fallback = Some((cur_dex, em.clone(), em.method_idx));
                            }
                        }
                        None => return Some((cur_dex, em.clone(), em.method_idx)),
                    }
                }
            }

            // Move to the superclass, which may live in a different DEX file.
            let sup = cd.superclass_idx;
            if sup == NO_INDEX {
                return fallback;
            }
            if dex.class_def(sup).is_some() {
                cur = sup;
                continue;
            }
            let desc = dex.type_name(sup).to_string();
            match self.resolve_class_any(&desc) {
                Some((d2, t2)) => {
                    cur_dex = d2;
                    cur = t2;
                }
                None => return fallback,
            }
        }
        fallback
    }

    /// Render a method's signature using `dex_id` as the index space.
    fn sig_in(&self, mid: &MethodId) -> String {
        let dex = &self.dexes[self.cur];
        let params: Vec<&str> = dex
            .proto(mid.proto)
            .map(|p| p.params.iter().map(|&t| dex.type_name(t)).collect())
            .unwrap_or_default();
        format!(
            "{}({}){}",
            dex.string(mid.name),
            params.join(", "),
            dex.return_type(mid)
        )
    }


    /// Whether this DEX defines the named class (dotted form).
pub fn has_class(&self, name: &str) -> bool {
        self.resolve_class_any(&Self::descriptor(name)).is_some()
    }

    /// Build a DEX type descriptor from a dotted class name.
    fn descriptor(dotted: &str) -> String {
        format!("L{};", dotted.replace('.', "/"))
    }

    fn class_of_method(&self, method_idx: MethodIdx) -> TypeIdx {
        self.d().methods[method_idx as usize].class as TypeIdx
    }

    // ---- execution -------------------------------------------------------

    pub fn exec(
        &mut self,
        dex_id: usize,
        method_idx: MethodIdx,
        args: Vec<Value>,
        depth: usize,
    ) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("call stack depth exceeded".into());
        }
        let saved = self.cur;
        self.cur = dex_id;

        let site = &self.d().methods[method_idx as usize];
        let class_idx = site.class as TypeIdx;
        let mname = self.d().string(site.name).to_string();
        // Resolve on the call site's own signature, so a name-only match can't
        // pick a sibling overload such as coerceAtLeast(D, D).
        let sig = self.sig_in(site);

        let result = match self.lookup(dex_id, class_idx, &mname, Some(&sig)) {
            Some((d, enc, m)) => self.exec_resolved(d, enc, m, args, depth),
            // A method_id names a class that need not be *defined* in the same
            // file. A static call across DEX boundaries is routine -- an
            // Activity in classes2.dex calling into a UI class in classes.dex
            // -- and the index alone cannot tell us that, so ask by descriptor
            // when the local file has nothing.
            None => {
                // class_name is dotted; resolve_class_any wants a descriptor.
                let desc = Self::descriptor(&self.d().class_name(class_idx));
                match self
                    .resolve_class_any(&desc)
                    .and_then(|(d2, t2)| self.lookup(d2, t2, &mname, Some(&sig)))
                {
                    Some((d, enc, m)) => self.exec_resolved(d, enc, m, args, depth),
                    None => self.builtin(method_idx, args),
                }
            }
        };
        self.cur = saved;
        result
    }

    /// Execute an already-resolved method. Callers that resolved with an arity
    /// constraint come through here so the method chosen at the call site is
    /// the one that actually runs.
    fn exec_resolved(
        &mut self,
        dex_id: usize,
        enc: EncodedMethod,
        method_idx: MethodIdx,
        args: Vec<Value>,
        depth: usize,
    ) -> Result<Value, String> {
        let saved = self.cur;
        self.cur = dex_id;
        let r = self.exec_body(enc, method_idx, args, depth);
        self.cur = saved;
        r
    }

    /// Interpret one method's bytecode.
    fn exec_body(
        &mut self,
        enc: EncodedMethod,
        method_idx: MethodIdx,
        args: Vec<Value>,
        depth: usize,
    ) -> Result<Value, String> {
        if enc.code_off == 0 {
            return self.builtin(method_idx, args);
        }
        let class_idx = self.class_of_method(method_idx);
        let mname = self.d().string(self.d().methods[method_idx as usize].name).to_string();
        let code = self.d().code_item(enc.code_off)?;
        let is_static = enc.access_flags & ACC_STATIC != 0;

        let mut regs: Vec<Value> = vec![Value::Void; code.registers_size as usize];

        // Callee params live in the *last* `ins_size` registers.
        let want = code.ins_size as usize;
        let base = code.registers_size as usize - want;
        let supplied = want;
        if args.len() < supplied {
            let mid = &self.d().methods[enc.method_idx as usize];
            let params: Vec<&str> = self
                .d()
                .proto(mid.proto)
                .map(|p| p.params.iter().map(|&t| self.d().type_name(t)).collect())
                .unwrap_or_default();
            return Err(format!(
                "{}: resolved to {}.{}({}) needing {} args, call supplied {}",
                mname,
                self.d().class_name(mid.class as TypeIdx),
                mname,
                params.join(", "),
                supplied,
                args.len()
            ));
        }
        // Lay arguments out in the frame. `ins_size` counts 32-bit words, so a
        // long or double parameter occupies two consecutive registers while the
        // caller passes it as a single value.
        let param_types: Vec<String> = self
            .d()
            .proto(self.d().methods[enc.method_idx as usize].proto)
            .map(|p| {
                p.params
                    .iter()
                    .map(|&t| self.d().type_name(t).to_string())
                    .collect()
            })
            .unwrap_or_default();

        let mut arg_i = 0usize;
        let mut slot = 0usize;
        if !is_static {
            if let Some(v) = args.first() {
                regs[base] = v.clone();
            }
            arg_i = 1;
            slot = 1;
        }
        for t in &param_types {
            if slot >= want {
                break;
            }
            if let Some(v) = args.get(arg_i) {
                regs[base + slot] = v.clone();
            }
            if t == "J" || t == "D" {
                regs[base + slot + 1] = Value::Void;
                slot += 2;
            } else {
                slot += 1;
            }
            arg_i += 1;
        }
        if arg_i > args.len() {
            return Err(format!(
                "{mname}: needs {arg_i} argument(s), call supplied {}",
                args.len()
            ));
        }

        let insns = &code.insns;
        let mut pc: usize = 0;
        // Holds the most recent call's result so `move-result` can pick it up,
        // which matters when d8 picks a different register for the pair.
        let mut pending: Value = Value::Void;

        // Convenience accessors. `unit` is the current 16-bit code unit.
        macro_rules! reg {
            // Out-of-range indices degrade to Void instead of aborting; a
            // desynchronised stream should not take the whole process down.
            ($i:expr) => {
                regs.get($i).cloned().unwrap_or(Value::Void)
            };
        }
        macro_rules! setreg {
            ($i:expr, $v:expr) => {
                if $i < regs.len() {
                    regs[$i] = $v
                }
            };
        }

        while pc < insns.len() {
            if STEPS.fetch_add(1, Ordering::Relaxed) >= max_steps() {
                return Err(format!(
                    "step budget exhausted ({}); the app is waiting on a frame \
                     clock that does not exist",
                    max_steps()
                ));
            }
            let unit = insns[pc];
            let op = (unit & 0xff) as u8;
            let aa = ((unit >> 8) & 0xff) as usize;
            let mut next = pc + insn_width(op);

            if tracing() {
                eprintln!("[{}.{}] pc={pc} op=0x{op:02x} aa={aa}", self.d().class_name(class_idx), mname);
            }

            match op {
                0x00 => {} // nop

                // ---- moves ----
                0x01 => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    setreg!(a, reg!(b));
                }
                0x04 => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    setreg!(a, reg!(b));
                }
                0x07 => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    setreg!(a, reg!(b));
                }
                0x02 | 0x05 | 0x08 => {
                    setreg!(aa, reg!(insns[pc + 1] as usize));
                }
                0x03 | 0x06 | 0x09 => {
                    // move/16, move-wide/16, move-object/16 (32x)
                    setreg!(insns[pc + 1] as usize, reg!(insns[pc + 2] as usize));
                }
                0x0a | 0x0b => setreg!(aa, pending.clone()), // move-result[-wide]
                0x0c | 0x0d => setreg!(aa, pending.clone()), // move-result-object/exception

                // ---- returns ----
                0x0e => return Ok(Value::Void), // return-void
                0x0f => return Ok(reg!(aa)),
                0x10 => return Ok(reg!(aa)),
                0x11 => return Ok(reg!(aa)),

                // ---- constants ----
                0x12 => {
                    // const/4 (11n): layout is `B|A|op`, so the destination
                    // is the low nibble of byte 1 and B is the literal.
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as i32;
                    let lit = if b > 7 { b - 16 } else { b };
                    setreg!(a, Value::Int(lit));
                }
                0x13 => setreg!(aa, Value::Int(insns[pc + 1] as i16 as i32)),
                0x14 => {
                    let lit = insns[pc + 1] as u32 | ((insns[pc + 2] as u32) << 16);
                    setreg!(aa, Value::Int(lit as i32));
                }
                0x15 => {
                    let lit = ((insns[pc + 1] as u32) << 16) as i32;
                    setreg!(aa, Value::Int(lit));
                }
                0x16 => setreg!(aa, Value::Long(insns[pc + 1] as i16 as i64)),
                0x17 => {
                    let lit = insns[pc + 1] as u32 | ((insns[pc + 2] as u32) << 16);
                    setreg!(aa, Value::Long(lit as i64));
                }
                0x18 => {
                    let mut lit: u64 = 0;
                    for k in 0..4 {
                        lit |= (insns[pc + 1 + k] as u64) << (16 * k);
                    }
                    setreg!(aa, Value::Long(lit as i64));
                }
                0x19 => {
                    let lit = ((insns[pc + 1] as u64) << 48) as i64;
                    setreg!(aa, Value::Long(lit));
                }
                0x1a => {
                    let s = self.d().string(insns[pc + 1] as u32).to_string();
                    let r = self.new_string(&s);
                    setreg!(aa, Value::Ref(r));
                }
                0x1b => {
                    let idx = insns[pc + 1] as u32 | ((insns[pc + 2] as u32) << 16);
                    let s = self.d().string(idx).to_string();
                    let r = self.new_string(&s);
                    setreg!(aa, Value::Ref(r));
                }
                0x1c => {
                    let t = insns[pc + 1] as TypeIdx;
                    setreg!(aa, Value::Int(t as i32));
                }

                // ---- object/array ----
                0x1f => {} // check-cast: no-op, we are dynamically typed
                0x1e => {} // monitor-enter
                0x1d => {} // monitor-exit
                0x20 => {
                    // instance-of (22c): vA = (vB is-a type@CCCC)
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let want = insns[pc + 1] as TypeIdx;
                    let obj = reg!(b).as_ref();
                    let mut result = 0;
                    if obj != 0 && (obj as usize) < self.heap.len() {
                        let mut cur = self.heap[obj as usize].class_idx;
                        while cur != NO_INDEX {
                            if cur == want {
                                result = 1;
                                break;
                            }
                            match self.d().class_def(cur) {
                                Some(cd) => cur = cd.superclass_idx,
                                None => break,
                            }
                        }
                    }
                    setreg!(a, Value::Int(result));
                }
                0x22 => {
                    let t = insns[pc + 1] as TypeIdx;
                    let r = self.alloc(t);
                    setreg!(aa, Value::Ref(r));
                }
                0x23 => {
                    // new-array (22c): `A|B|op CCCC` — A is the destination,
                    // B holds the length, CCCC is the element type.
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let t = insns[pc + 1] as TypeIdx;
                    let len = reg!(b).as_int().max(0) as usize;
                    let r = self.new_array(t, len);
                    setreg!(a, Value::Ref(r));
                }
                0x24 => {
                    // filled-new-array {..}, type@CCCC (35c shape). Unlike
                    // invoke, the type sits in unit 1 and the register list
                    // in unit 2.
                    let (idxs, _, _) = self.decode_35c(insns, pc);
                    let elems: Vec<Value> = idxs
                        .iter()
                        .map(|&i| regs.get(i).cloned().unwrap_or(Value::Void))
                        .collect();
                    let t = insns[pc + 1] as TypeIdx;
                    let r = self.new_array(t, elems.len());
                    self.heap[r as usize].elems = Some(elems);
                    setreg!(aa, Value::Ref(r));
                }
                0x25 => {
                    // filled-new-array/range {vC..vC+AA-1}, type@BBBB (3rc)
                    let count = ((unit >> 8) & 0xff) as usize;
                    let t = insns[pc + 1] as TypeIdx;
                    let start = insns[pc + 2] as usize;
                    let elems: Vec<Value> = (start..start + count)
                        .map(|i| reg!(i))
                        .collect();
                    let r = self.new_array(t, elems.len());
                    self.heap[r as usize].elems = Some(elems);
                    setreg!(aa, Value::Ref(r));
                }
                0x21 => {
                    // array-length (12x): `B|A|op`, destination is the low nibble.
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let rv = reg!(b).as_ref();
                    let len = self
                        .obj(rv)
                        .and_then(|o| o.elems.as_ref())
                        .map(|e| e.len() as i32)
                        .unwrap_or(0);
                    setreg!(a, Value::Int(len));
                }
                0x27 => {
                    // throw
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let v = reg!(a);
                    let sig = self.sig_in(&self.d().methods[enc.method_idx as usize]);
                    let size = self.d().code_item(enc.code_off).map(|c| c.insns.len()).unwrap_or(0);
                    return Err(format!(
                        "uncaught {} at pc={pc} in {sig} [method#{}, {} units, depth {depth}]",
                        self.exception_text(&v),
                        enc.method_idx,
                        size
                    ));
                }

                // ---- arrays ----
                // 0x44-0x4a aget family, 0x4b-0x51 aput family. The type
                // variants all move the same Value representation.
                0x44..=0x4a => {
                    // aget / aget-wide / aget-object / aget-boolean
                    let bb = (insns[pc + 1] & 0xff) as usize;
                    let cc = (insns[pc + 1] >> 8) as usize;
                    let arr = reg!(bb).as_ref();
                    let i = reg!(cc).as_int() as usize;
                    let v = self
                        .obj(arr)
                        .and_then(|o| o.elems.as_ref())
                        .and_then(|e| e.get(i).cloned())
                        .unwrap_or(Value::Int(0));
                    setreg!(aa, v);
                }
                0x4b..=0x51 => {
                    // aput / aput-wide / aput-object / aput-boolean
                    let bb = (insns[pc + 1] & 0xff) as usize;
                    let cc = (insns[pc + 1] >> 8) as usize;
                    let arr = reg!(bb).as_ref();
                    let i = reg!(cc).as_int() as usize;
                    let v = reg!(aa);
                    if arr != 0 {
                        if let Some(e) = self.heap[arr as usize].elems.as_mut() {
                            if i < e.len() {
                                e[i] = v;
                            }
                        }
                    }
                }

                // ---- instance fields (22c: `A|B|op CCCC`, 2 units) ----
                // 0x52-0x58 iget family, 0x59-0x5f iput family.
                0x52..=0x58 => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let fidx = insns[pc + 1] as FieldIdx;
                    let obj = reg!(b).as_ref();
                    let v = self
                        .obj(obj)
                        .and_then(|o| o.fields.get(&fidx).cloned())
                        .unwrap_or(Value::Int(0));
                    setreg!(a, v);
                }
                0x59..=0x5f => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let fidx = insns[pc + 1] as FieldIdx;
                    let obj = reg!(b).as_ref();
                    let v = reg!(a);
                    if obj != 0 {
                        self.heap[obj as usize].fields.insert(fidx, v);
                    }
                }

                // ---- static fields ----
                0x60..=0x66 => {
                    let fidx = insns[pc + 1] as FieldIdx;
                    let v = self.statics.get(&fidx).cloned().unwrap_or(Value::Int(0));
                    setreg!(aa, v);
                }
                0x67..=0x6d => {
                    let fidx = insns[pc + 1] as FieldIdx;
                    let v = reg!(aa);
                    self.statics.insert(fidx, v);
                }

                // ---- invokes ----
                0x6e..=0x72 => {
                    let (idxs, _, mid) = self.decode_35c(insns, pc);
                    let args: Vec<Value> = idxs.iter().map(|&i| reg!(i)).collect();
                    // The invoke itself does not name a destination register —
                    // the following `move-result` does, and the verifier always
                    // pairs them. So park the result in `pending` and let
                    // move-result place it, rather than guessing a register.
                    let r = self.dispatch(self.cur, op, mid, args, depth)?;
                    pending = r;
                }
                0x74..=0x78 => {
                    let (mid, start, count) = self.decode_3rc(insns, pc);
                    let args: Vec<Value> = (start..start + count).map(|i| reg!(i)).collect();
                    let r = self.dispatch(self.cur, op, mid, args, depth)?;
                    pending = r;
                }

                // ---- control flow ----
                0x28 => {
                    let off = ((unit >> 8) & 0xff) as i8 as i32;
                    next = (pc as i32 + off) as usize;
                }
                0x29 => {
                    let off = insns[pc + 1] as i16 as i32;
                    next = (pc as i32 + off) as usize;
                }
                0x2a => {
                    let off = (insns[pc + 1] as i32) | ((insns[pc + 2] as i32) << 16);
                    next = (pc as i32 + off) as usize;
                }
                0x2b => {
                    // packed-switch: keys are a dense run, so find the slot.
                    let q = Self::switch_payload(insns, pc)
                        .ok_or("packed-switch payload not found")?;
                    let size = insns[q + 1] as usize;
                    let keys = q + 2;
                    let val = reg!(aa).as_int() as u32;
                    for i in 0..size {
                        let k = insns[keys + i * 2] as u32 | ((insns[keys + i * 2 + 1] as u32) << 16);
                        if k == val {
                            let t = keys + size * 2 + i * 2;
                            let off = insns[t] as i16 as i32;
                            next = (pc as i32 + off) as usize;
                            break;
                        }
                    }
                }
                0x2c => {
                    // sparse-switch: parallel key and target tables.
                    let q = Self::switch_payload(insns, pc)
                        .ok_or("sparse-switch payload not found")?;
                    let size = insns[q + 1] as usize;
                    let keys = q + 2;
                    let targets = keys + size * 2;
                    let val = reg!(aa).as_int() as u32;
                    for i in 0..size {
                        let k = insns[keys + i * 2] as u32 | ((insns[keys + i * 2 + 1] as u32) << 16);
                        if k == val {
                            let off = insns[targets + i * 2] as i16 as i32;
                            next = (pc as i32 + off) as usize;
                            break;
                        }
                    }
                }
                0x2d..=0x31 => {
                    // cmpl-float/cmpg-float/cmpl-double/cmpg-double/cmp-long
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let x = reg!(a).as_long();
                    let y = reg!(b).as_long();
                    let c = if x < y {
                        -1
                    } else if x == y {
                        0
                    } else {
                        1
                    };
                    // The g-variants map NaN to 1, the l-variants to -1.
                    let c = if matches!(op, 0x2d | 0x2f) {
                        c
                    } else if matches!(op, 0x2e | 0x30) && c == 0 {
                        0
                    } else {
                        c
                    };
                    setreg!(a, Value::Int(c));
                }

                // if-<cond> vA, vB, +CCCC   (22t)
                0x32..=0x37 => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let off = insns[pc + 1] as i16 as i32;
                    let (rv_x, rv_y) = (reg!(a), reg!(b));
                    // Equality must compare the register's actual contents.
                    // Coercing to a number first turns every object reference
                    // into 0, which makes `a == b` true for all a and b --
                    // and object identity checks are everywhere in real app
                    // code. Dalvik compares floats bitwise, so NaN equals
                    // itself here too; match that.
                    let bits = |v: &Value| -> u64 {
                        match v {
                            Value::Void => 0,
                            Value::Int(i) => *i as u32 as u64,
                            Value::Ref(r) => *r as u64,
                            Value::Long(l) => *l as u64,
                            Value::Float(f) => f.to_bits() as u64,
                            Value::Double(d) => d.to_bits(),
                        }
                    };
                    let (x, y) = (rv_x.as_long(), rv_y.as_long());
                    let take = match op {
                        0x32 => bits(&rv_x) == bits(&rv_y),
                        0x33 => bits(&rv_x) != bits(&rv_y),
                        0x34 => x < y,
                        0x35 => x >= y,
                        0x36 => x > y,
                        _ => x <= y,
                    };
                    if take {
                        next = (pc as i32 + off) as usize;
                    }
                }
                // if-<cond>z vAA, +BBBB   (21t)
                0x38..=0x3d => {
                    let off = insns[pc + 1] as i16 as i32;
                    let v = reg!(aa);
                    // A reference is tested for null, not coerced to a number.
                    // Coercion returns 0 for every object, so `if (x != null)`
                    // was always taken -- which made non-null objects behave
                    // as null everywhere, and is invisible until an app
                    // asserts on the difference.
                    let take = match &v {
                        Value::Ref(r) => match op {
                            0x38 => *r == 0,
                            _ => *r != 0,
                        },
                        _ => {
                            let x = v.as_long();
                            match op {
                                0x38 => x == 0,
                                0x39 => x != 0,
                                0x3a => x < 0,
                                0x3b => x >= 0,
                                0x3c => x > 0,
                                _ => x <= 0,
                            }
                        }
                    };
                    if take {
                        next = (pc as i32 + off) as usize;
                    }
                }

                // ---- integer arithmetic ----
                0x90..=0xa5 | 0xb0..=0xc5 => {
                    // 3addr (23x) is vA = vB op vCC; 2addr (12x) is vA = vA op vB.
                    // The long variants sit at 0x9b-0xa5 and 0xbb-0xc5.
                    let (a, b, c, is_long) = if op >= 0xb0 {
                        let a = ((unit >> 8) & 0x0f) as usize;
                        let b = ((unit >> 12) & 0x0f) as usize;
                        (a, b, b, op >= 0xbb)
                    } else {
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let c = (insns[pc + 1] >> 8) as usize;
                        (aa, b, c, op >= 0x9b)
                    };
                    let v = if op >= 0xb0 {
                        self.arith(op, reg!(a), reg!(b), is_long)
                    } else {
                        self.arith(op, reg!(b), reg!(c), is_long)
                    };
                    setreg!(a, v);
                }

                0xd0..=0xd7 => {
                    // <op>-int/lit16 vA, vB, #+CCCC  (`A|B|op CCCC`, 2 units)
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let lit = insns[pc + 1] as i16 as i32;
                    let v = self.arith(op, reg!(b), Value::Int(lit), false);
                    setreg!(a, v);
                }
                0xd8..=0xe2 => {
                    // <op>-int/lit8 vAA, vBB, #+CC  (2 units: the register is in
                    // the low byte of unit 1, the signed literal in the high byte)
                    let bb = (insns[pc + 1] & 0xff) as usize;
                    let lit = ((insns[pc + 1] >> 8) & 0xff) as u8 as i8 as i32;
                    let v = self.arith(op, reg!(bb), Value::Int(lit), false);
                    setreg!(aa, v);
                }

                // ---- unary ops (12x: `B|A|op`, so vA = op vB) ----
                0x7b..=0x8f => {
                    let a = ((unit >> 8) & 0x0f) as usize;
                    let b = ((unit >> 12) & 0x0f) as usize;
                    let v = self.unop(op, reg!(b));
                    setreg!(a, v);
                }

                other => {
                    return Err(format!(
                        "unimplemented opcode 0x{other:02x} at pc={pc} in {}.{}",
                        self.d().class_name(class_idx),
                        mname
                    ));
                }
            }
            pc = next;
        }
        Ok(Value::Void)
    }

    /// Unary operations and numeric conversions (12x forms).
    ///
    /// Float/double conversions are passed through unchanged: the interpreter
    /// doesn't model Java's narrowing rules for them yet, but they only appear
    /// in code that is already doing float math.
    fn unop(&self, op: u8, x: Value) -> Value {
        match op {
            0x7b => Value::Int(x.as_int().wrapping_neg()),
            0x7c => Value::Int(!x.as_int()),
            0x7d => Value::Long(x.as_long().wrapping_neg()),
            0x7e => Value::Long(!x.as_long()),
            0x81 => Value::Long(x.as_int() as i64),  // int-to-long
            0x84 => Value::Int(x.as_long() as i32),  // long-to-int
            0x8d => Value::Int(x.as_int() as i8 as i32),   // int-to-byte
            0x8e => Value::Int(x.as_int() as u16 as i32),  // int-to-char
            0x8f => Value::Int(x.as_int() as i16 as i32),  // int-to-short
            _ => x,
        }
    }

    /// Locate the data payload that precedes a switch instruction. The payload sits
/// immediately before it, optionally separated by nop padding, so walk back
/// over zero words.
fn switch_payload(insns: &[u16], pc: usize) -> Option<usize> {
    let mut q = pc;
    while q > 0 {
        q -= 1;
        if insns[q] != 0x0000 {
            return Some(q);
        }
    }
    None
}

/// How many saved cursors sit at the front of a Composer's slot vector. They
/// are a run of Ints, and slots hold references, so the two never collide.
fn stack_depth(stack: &[Value]) -> usize {
    stack.iter().take_while(|v| matches!(v, Value::Int(_))).count()
}

fn arith(&self, op: u8, x: Value, y: Value, is_long: bool) -> Value {
        if is_long {
            let a = x.as_long();
            let b = y.as_long();
            return Value::Long(match op {
                0x9b | 0xbb => a.wrapping_add(b),
                0x9c | 0xbc => a.wrapping_sub(b),
                0x9d | 0xbd => a.wrapping_mul(b),
                0x9e | 0xbe => {
                    if b == 0 { 0 } else { a.wrapping_div(b) }
                }
                0x9f | 0xbf => {
                    if b == 0 { 0 } else { a.wrapping_rem(b) }
                }
                0xa0 | 0xc0 => a & b,
                0xa1 | 0xc1 => a | b,
                0xa2 | 0xc2 => a ^ b,
                0xa3 | 0xc3 => a.wrapping_shl(b as u32),
                0xa4 | 0xc4 => a.wrapping_shr(b as u32),
                0xa5 | 0xc5 => ((a as u64).wrapping_shr(b as u32)) as i64,
                _ => a,
            });
        }
        let a = x.as_int();
        let b = y.as_int();
        Value::Int(match op {
            // 3addr (0x90..=0x9a) and 2addr (0xb0..=0xba) families
            0x90 | 0xb0 => a.wrapping_add(b),
            0x91 | 0xb1 => a.wrapping_sub(b),
            0x92 | 0xb2 => a.wrapping_mul(b),
            0x93 | 0xb3 => {
                if b == 0 { 0 } else { a.wrapping_div(b) }
            }
            0x94 | 0xb4 => {
                if b == 0 { 0 } else { a.wrapping_rem(b) }
            }
            0x95 | 0xb5 => a & b,
            0x96 | 0xb6 => a | b,
            0x97 | 0xb7 => a ^ b,
            0x98 | 0xb8 => a.wrapping_shl(b as u32),
            0x99 | 0xb9 => a.wrapping_shr(b as u32),
            0x9a | 0xba => ((a as u32).wrapping_shr(b as u32)) as i32,
            // lit16 family — x is the register, y the literal
            0xd0 => a.wrapping_add(b),
            0xd1 => b.wrapping_sub(a), // rsub: vA = lit - vB
            0xd2 => a.wrapping_mul(b),
            0xd3 => {
                if b == 0 { 0 } else { a.wrapping_div(b) }
            }
            0xd4 => {
                if b == 0 { 0 } else { a.wrapping_rem(b) }
            }
            0xd5 => a & b,
            0xd6 => a | b,
            0xd7 => a ^ b,
            // lit8 family — x is the register, y the literal
            0xd8 => a.wrapping_add(b),
            0xd9 => b.wrapping_sub(a), // rsub: vA = lit - vB
            0xda => a.wrapping_mul(b),
            0xdb => {
                if b == 0 { 0 } else { a.wrapping_div(b) }
            }
            0xdc => {
                if b == 0 { 0 } else { a.wrapping_rem(b) }
            }
            0xdd => a & b,
            0xde => a | b,
            0xdf => a ^ b,
            0xe0 => a.wrapping_shl(b as u32),
            0xe1 => a.wrapping_shr(b as u32),
            0xe2 => ((a as u32).wrapping_shr(b as u32)) as i32,
            _ => a,
        })
    }

    /// Decode a format 35c instruction: returns (arg register indices, count,
    /// method index).
    ///
    /// Verified against d8 output for 1- through 5-register call sites: byte1
    /// carries A in bits 6-4 and G in bits 3-0, and the third code unit holds
    /// C|D in its low byte and E|F in its high byte.
    fn decode_35c(&self, insns: &[u16], pc: usize) -> (Vec<usize>, usize, MethodIdx) {
        let byte1 = (insns[pc] >> 8) as u8;
        let a = ((byte1 >> 4) & 0x07) as usize;
        let g = (byte1 & 0x0f) as usize;
        let mid = insns[pc + 1] as MethodIdx;
        let u = insns[pc + 2];
        let c = (u & 0x0f) as usize;
        let d = ((u >> 4) & 0x0f) as usize;
        let e = ((u >> 8) & 0x0f) as usize;
        let f = ((u >> 12) & 0x0f) as usize;
        let all = [c, d, e, f, g];
        (all[..a.min(5)].to_vec(), a.min(5), mid)
    }

    fn decode_3rc(&self, insns: &[u16], pc: usize) -> (MethodIdx, usize, usize) {
        let count = ((insns[pc] >> 8) as u8) as usize;
        let mid = insns[pc + 1] as MethodIdx;
        let start = insns[pc + 2] as usize;
        (mid, start, count)
    }

    /// Resolve a call site. `invoke-virtual` dispatches on the receiver's
    /// runtime class; every other form binds to the method named directly.
    /// Framework methods we answer directly, *before* method resolution.
    ///
    /// Some AndroidX methods ship real bytecode that depends on framework state
    /// we don't model — `Lifecycle$State.isAtLeast` compares ordinals against a
    /// static `STARTED` we can never populate. Resolving those would run the
    /// real code and fail, so they are intercepted here instead.
    fn framework_override(&mut self, class: &str, name: &str, args: &[Value]) -> Option<Value> {
        let r = match (class, name) {
            ("androidx/lifecycle/LifecycleOwner", "getLifecycle") => {
                Value::Ref(self.new_holder("Landroidx/lifecycle/Lifecycle;"))
            }
            ("androidx/lifecycle/Lifecycle", "getCurrentState") => {
                Value::Ref(self.new_holder("Landroidx/lifecycle/Lifecycle$State;"))
            }
            // The Activity lifecycle itself. Fragment/ViewModel setup needs a
            // real FragmentManager and windowing, so the lifecycle callbacks
            // become no-ops and the Activity's own onCreate does the work.
            (
                "androidx/fragment/app/FragmentActivity"
                | "androidx/activity/ComponentActivity"
                | "android/app/Activity",
                "onCreate" | "onStart" | "onResume" | "onPause" | "onStop" | "onDestroy"
                | "onNewIntent" | "onSaveInstanceState" | "onRestoreInstanceState",
            ) => Value::Void,
            ("androidx/lifecycle/ViewModelProvider", "get") => {
                Value::Ref(0)
            }
            // Jetpack Compose. setContent builds a composable tree and hands it
            // Jetpack Compose. setContent is normally handed a ComposeView
            // and a WindowRecomposer -- real Android view plumbing we have no
            // window for. But the content lambda itself is ordinary bytecode,
            // and it is the thing that actually builds the UI, so invoke it
            // directly against our own Composer. Skipping the View is the
            // whole trick: no window means nothing would otherwise ever
            // compose.
            (
                "androidx/activity/compose/ComponentActivityKt",
                "setContent$default" | "setContent",
            ) => return self.compose_content(args),
            // The Compose group protocol. The compiler brackets each
            // composable's emissions with start/endRestartGroup; a Composer
            // that hands itself straight back and reports nothing changed keeps
            // the structure flat, but the composable body runs to the end --
            // which is the point. Claiming "changed" would restart the group
            // forever and re-emit nothing.
            (
                "androidx/compose/runtime/Composer",
                "startRestartGroup" | "startReplaceableGroup" | "startRestartGroupPassingParams",
            ) => {
                let c = args.first().map(|v| v.as_ref()).unwrap_or(0);
                self.slot_push(c);
                args.first().cloned().unwrap_or(Value::Ref(0))
            }
            (
                "androidx/compose/runtime/Composer",
                "endRestartGroup" | "endReplaceableGroup" | "endRestartGroupPassingParams",
            ) => {
                let c = args.first().map(|v| v.as_ref()).unwrap_or(0);
                self.slot_pop(c);
                Value::Ref(self.new_holder("Landroidx/compose/runtime/internal/ScopeUpdateScope;"))
            }
            // A first composition compares against an empty slot table, so
            // nothing has changed. Reporting a real comparison looked more
            // correct and went less far: a composable that decides it was
            // invalidated then re-emits, and with no slot table to re-emit
            // into it loses its place. Optimistic is the safe answer here.
            ("androidx/compose/runtime/Composer", "changed" | "changedInstance") => Value::Int(0),
            // The gate every composable opens with. Answering false makes the
            // body skip itself entirely, which is why the tree came out empty.
            ("androidx/compose/runtime/Composer", "shouldExecute") => Value::Int(1),
            // `remember` reads the current slot then writes the new value
            // back and moves on. Answering reads with a constant null, as this
            // used to, means nothing is ever remembered.
            ("androidx/compose/runtime/Composer", "rememberedValue") => {
                let c = args.first().map(|v| v.as_ref()).unwrap_or(0);
                return Some(self.slot_peek(c));
            }
            ("androidx/compose/runtime/Composer", "updateRememberedValue") => {
                let c = args.first().map(|v| v.as_ref()).unwrap_or(0);
                let v = args.get(1).cloned().unwrap_or(Value::Ref(0));
                self.slot_write(c, v);
                return Some(Value::Void);
            }
            ("androidx/compose/runtime/Composer$Companion", "getEmpty") => {
                Value::Ref(self.empty)
            }
            ("androidx/compose/runtime/ComposerKt", "isTraceInProgress") => Value::Int(0),
            ("androidx/compose/runtime/Composer", "skipToGroupEnd") => Value::Void,
            // Replacement groups bracket content the runtime may skip. None is
            // ever skipped here, so open and close them as no-ops -- but they
            // still have to be accepted, or the composable aborts on a missing
            // method rather than producing its nodes.
            ("androidx/compose/runtime/Composer", "startReplaceGroup") => Value::Void,
            ("androidx/compose/runtime/Composer", "endReplaceGroup") => Value::Void,
            // Side effects run after composition applies. Running them during
            // composition would call back into a tree that does not exist yet.
            ("androidx/compose/runtime/Composer", "recordSideEffect") => Value::Void,
            ("androidx/compose/runtime/Composer", "getCompositeKeyHashCode") => {
                Value::Long(0)
            }
            ("androidx/compose/runtime/Composer", "getApplyCoroutineContext") => {
                Value::Ref(self.new_holder("Lkotlin/coroutines/CoroutineContext;"))
            }
            // The provided value for a CompositionLocal. With no provider in
            // the tree the local itself is the closest truthful answer, and it
            // keeps callers off their null checks.
            ("androidx/compose/runtime/Composer", "consume") => {
                args.get(1).cloned().unwrap_or(Value::Ref(0))
            }
            ("androidx/compose/runtime/ScopeUpdateScope", "updateScope") => Value::Void,
            // A checkcast guard Kotlin emits before invoking a Function2 of a
            // known arity. Passing the object through is the whole job. The
            // arity is the second argument because this one is static, so the
            // object is first -- reading args[1] returns the arity and the
            // composable body never gets invoked.
            ("kotlin/jvm/internal/TypeIntrinsics", "beforeCheckcastToFunctionOfArity") => {
                args.first().cloned().unwrap_or(Value::Ref(0))
            }
            // The ComposableLambda returned here is the implementation class,
            // not the interface: the interface declares no methods, so a
            // lookup starting from it finds nothing to call.
            (
                "androidx/compose/runtime/internal/ComposableLambdaKt",
                "composableLambdaInstance" | "composableLambdaInstance$default",
            ) => {
                let (owner, ci) = self.composable_lambda_class();
                let l = self.alloc(ci);
                self.heap[l as usize].elems = Some(Vec::new());
                // `_block` is the actual lambda, and it is the third argument:
                // this is a static file-facade factory, so there is no receiver
                // shifting the positions. Without it the generated invoke()
                // calls null and the composable body never runs.
                if let (Some(block), Some(fi)) = (
                    args.get(2).cloned(),
                    self.field_index_in(owner, ci, "_block"),
                ) {
                    self.heap[l as usize].fields.insert(fi, block);
                }
                Value::Ref(l)
            }
            // android.content.Context. These have no real answer without a
            // system server, but stable non-null placeholders keep callers off
            // their own null paths.
            _ if class.ends_with("Context") || class.ends_with("Activity") => match name {
                "getApplicationContext" | "getApplication" => self.application(),
                "getResources" => self.service("Landroid/content/res/Resources;"),
                "getSystemService" => {
                    let n = args
                        .get(1)
                        .map(|v| v.as_ref())
                        .and_then(|i| self.heap.get(i as usize))
                        .and_then(|o| o.string.clone())
                        .unwrap_or_default();
                    Value::Ref(self.service_descriptor(&n))
                }
                "getPackageName" => Value::Ref(self.new_string("org.fdroid")),
                // ComponentActivity's constructor null-checks these before it
                // stores them, so returning null aborts startup with a
                // NullPointerException rather than anything useful. Keyed per
                // class so two Activities do not share one instance.
                "getLifecycle" | "getViewModelStore" | "getSavedStateRegistryOwner"
                | "getOnBackPressedDispatcher" => {
                    self.service(&format!("L{class};$member;"))
                }
                _ => return None,
            },
            // R8 outlines `checkNotNull` into a synthetic single-call helper
            // that reports the failure and then throws null. These guard
            // framework state we deliberately do not model -- ComponentActivity
            // asserting its own Lifecycle exists, for instance -- so failing
            // them tells us nothing we can act on and only aborts startup.
            // Matched on the shape R8 guarantees, not on one app's name.
            _ if class.contains("ExternalSyntheticBUOutline")
                && matches!(name, "m" | "m0" | "$$m" | "lambda$") =>
            {
                Value::Void
            }
            // SnapshotKt.readable/writable decide whether a state may be read
            // under the current snapshot, and throw KotlinNothingValueException
            // when it may not. With no snapshot system there is never a valid
            // snapshot, so every state read fails and the composition dies at
            // the first one. Passing the record straight back is what these
            // return on the happy path.
            (
                "androidx/compose/runtime/snapshots/SnapshotKt",
                "readable" | "writable",
            ) => args.first().cloned().unwrap_or(Value::Ref(0)),
            // Compose's snapshot consistency checks. These only run when the
            // snapshot machinery is unhappy -- a state being read outside a
            // valid snapshot -- and there is no snapshot here to be unhappy
            // about. Left to run, the first one aborts the composition with an
            // IllegalStateException that says nothing we can act on.
            _ if class.starts_with("androidx/compose/runtime/snapshots/SnapshotKt")
                && name.ends_with("Error") =>
            {
                Value::Void
            }
            // An inline value class is mangled with a dash; this one packs
            // the window's width and height as two floats. A window that does
            // not exist has no size, and zero is what an absent one means.
            _ if class.ends_with("WindowInfo") && name.starts_with("getContainerSize") => {
                Value::Long(0)
            }
            // Kotlin's lateinit guard. The getter compiles to
            //   sfield; if-eqz -> throwUninitializedProperty
            // so the read already happened by the time we're called; returning
            // lets the caller use that (unset) value rather than abort.
            // Both spellings exist in the wild: the property getter calls
            // throwUninitializedPropertyAccessException, and the $delegate
            // path calls throwUninitializedProperty.
            ("kotlin/jvm/internal/Intrinsics", "throwUninitializedProperty")
            | ("kotlin/jvm/internal/Intrinsics", "throwUninitializedPropertyAccessException")
            | ("kotlin/jvm/internal/Intrinsics", "checkNotNullExpressionValue") => Value::Void,
            ("kotlin/jvm/internal/Intrinsics", "checkNotNull") => {
                args.first().cloned().unwrap_or(Value::Ref(0))
            }
            // Dagger/Hilt assertion helpers. They exist to abort startup when
            // a component graph is misconfigured; since there is no real graph
            // here, treating them as satisfied is what lets startup continue.
            ("dagger/hilt/internal/Preconditions", "checkState") | ("dagger/internal/Preconditions", "checkState") => {
                Value::Void
            }
            ("dagger/hilt/internal/Preconditions", "checkNotNull")
            | ("dagger/internal/Preconditions", "checkNotNull")
            | ("dagger/hilt/internal/Preconditions", "checkBuilderRequirement") => {
                args.first().cloned().unwrap_or(Value::Ref(0))
            }
            _ => {
                // `isAtLeast` is checked during registration, where AndroidX
                // requires the owner to *not* be STARTED yet ("must call
                // register before they are STARTED"). We present a fresh,
                // pre-started lifecycle, so the answer is false. Bridge methods
                // vary the declaring class, so match on the name.
                if name == "isAtLeast" {
                    Value::Int(0)
                } else {
                    return None;
                }
            }
        };
        Some(r)
    }

    /// Resolve a call site and invoke it.
    ///
    /// Non-virtual forms already name the exact method in their method_id, so
    /// they are called directly. `invoke-virtual` has to find the receiver
    /// class's override, matched on the call site's full signature.
    fn dispatch(
        &mut self,
        dex_id: usize,
        op: u8,
        mid: MethodIdx,
        args: Vec<Value>,
        depth: usize,
    ) -> Result<Value, String> {
        let saved = self.cur;
        self.cur = dex_id;

        // Intercept framework methods whose real bytecode we can't satisfy.
        {
            let site = &self.d().methods[mid as usize];
            let cls = self.d().class_name(site.class as TypeIdx);
            let mname = self.d().string(site.name).to_string();
            if let Some(v) = self.framework_override(&cls, &mname, &args) {
                self.cur = saved;
                return Ok(v);
            }
        }

        // invoke-interface resolves on the receiver's runtime class, exactly
        // like invoke-virtual -- the method_id names an abstract interface
        // method with no code, so dispatching straight to it silently does
        // nothing. That swallowed every Kotlin lambda and every Compose
        // composable body.
        let result = if matches!(op, 0x6e | 0x72 | 0x74 | 0x78) {
            let name = self.d().string(self.d().methods[mid as usize].name).to_string();
            let sig = self.sig_in(&self.d().methods[mid as usize]);
            // Resolve against the receiver's own file, not the caller's. A
            // Compose lambda allocated in classes2.dex is still being invoked
            // from a method in classes.dex, and reading its class index there
            // names an unrelated class -- so the call silently found nothing.
            let recv = args
                .first()
                .and_then(|v| self.obj(v.as_ref()))
                .map(|o| (o.dex, o.class_idx));
            match recv.and_then(|(rd, rc)| self.lookup(rd, rc, &name, Some(&sig))) {
                Some((d, enc, m)) => self.exec_resolved(d, enc, m, args, depth + 1),
                None => self.exec(dex_id, mid, args, depth + 1),
            }
        } else {
            self.exec(dex_id, mid, args, depth + 1)
        };
        self.cur = saved;
        result
    }

    /// Lazily attach an `elems` payload to an object we allocated as null
    /// (framework constructors we don't model often return uninitialised).
    fn ensure_holder(&mut self, r: u32, class: &str) -> u32 {
        let existing = self.obj(r).map(|o| o.elems.is_some()).unwrap_or(false);
        if existing {
            return r;
        }
        let t = self.type_index_for(&Self::descriptor(&class.replace('/', ".")));
        let idx = self.alloc(t);
        self.heap[idx as usize].elems = Some(Vec::new());
        idx
    }

    fn ensure_cell(&mut self, r: u32, class: &str) -> u32 {
        let existing = self.obj(r).map(|o| o.value.is_some()).unwrap_or(false);
        if existing {
            return r;
        }
        self.new_cell(&Self::descriptor(&class.replace('/', ".")), Value::Int(0))
    }

    /// Borrow a heap object by reference, rejecting null and out-of-range refs.
///
/// Stubbed framework methods return null or placeholder values, so the app's
/// own code will happily dereference something we never allocated. Returning
/// `None` lets execution degrade instead of aborting the process.
fn obj(&self, r: u32) -> Option<&Object> {
    if r == 0 || r as usize >= self.heap.len() {
        return None;
    }
    self.heap.get(r as usize)
}

/// Render one `%`-conversion for the `String.format` shim.
    fn format_one(&self, spec: char, width: usize, v: &Value) -> String {
        let body = match spec {
            'd' | 'x' | 'X' | 'o' => match spec {
                'x' => format!("{:x}", v.as_int()),
                'X' => format!("{:X}", v.as_int()),
                'o' => format!("{:o}", v.as_int()),
                _ => v.as_int().to_string(),
            },
            'f' => format!("{}", v.as_int() as f64),
            'b' => (v.as_int() != 0 || matches!(v, Value::Ref(r) if *r != 0)).to_string(),
            _ => self.describe(v),
        };
        if body.chars().count() >= width {
            body
        } else {
            format!("{}{}", " ".repeat(width - body.chars().count()), body)
        }
    }

    /// Name a thrown object's class, which is usually the most useful part of the
    /// diagnostic — it tells us which framework call the app was attempting.
    fn exception_text(&self, v: &Value) -> String {
        match self.obj(v.as_ref()) {
            Some(o) => self.d().class_name(o.class_idx).to_string(),
            None => self.describe(v),
        }
    }

    /// Log framework methods that fell through to the default return value.
    ///
    /// This is how you discover which parts of the Android surface an app
    /// actually needs: run it with `APKNATIVE_TRACE_MISSING=1` and collect the
    /// set. Enable it with `APKNATIVE_TRACE_MISSING=1`.
    fn note_missing(&self, class: &str, sig: &str) {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("APKNATIVE_TRACE_MISSING").is_some()) {
            return;
        }
        eprintln!("missing shim: {class}.{sig}");
    }

    fn describe(&self, v: &Value) -> String {
        match self.string_text(v) {
            Some(s) => s.to_string(),
            None => match v {
                Value::Int(i) => i.to_string(),
                Value::Long(l) => l.to_string(),
                Value::Ref(0) | Value::Void => "null".into(),
                Value::Ref(r) => format!("object#{r}"),
                _ => "?".into(),
            },
        }
    }

    // ---- built-in shims --------------------------------------------------

    fn builtin(&mut self, mid: MethodIdx, args: Vec<Value>) -> Result<Value, String> {
        let m = &self.d().methods[mid as usize];
        let class = self.d().class_name(m.class as TypeIdx);
        let name = self.d().string(m.name).to_string();
        let ret = self.d().type_name(self.d().proto(m.proto).unwrap().ret).to_string();
        // The callee's own signature. javac calls StringBuilder.append(Z)
        // directly, so the only place the boolean-ness survives is here -- a
        // Value::Int(1) is otherwise indistinguishable from the integer 1.
        let sig = self.sig_in(m);

        if name == "<init>" || name == "<clinit>" {
            return Ok(Value::Void);
        }

        // System.out / System.err
        if class == "java/io/PrintStream" && (name == "println" || name == "print") {
            let text = args
                .get(1)
                .map(|v| self.describe(v))
                .unwrap_or_else(|| "null".into());
            if name == "println" {
                println!("{text}");
            } else {
                print!("{text}");
            }
            return Ok(Value::Void);
        }

        // String methods. CharSequence reaches here too: it declares `length()`
        // and a CharSequence is backed by the same string data.
        if class == "java/lang/String" || class == "java/lang/CharSequence" {
            let me = self
                .string_text(args.first().unwrap_or(&Value::Void))
                .map(|s| s.to_string());
            match name.as_str() {
                "getClass" => {
                    let t = self
                        .obj(args.first().map(|v| v.as_ref()).unwrap_or(0))
                        .map(|o| o.class_idx)
                        .unwrap_or(NO_INDEX);
                    let c = self.new_class_object(t);
                    return Ok(Value::Ref(c));
                }
                "substring" => {
                    let s = me.unwrap_or_default();
                    let ch: Vec<char> = s.chars().collect();
                    let a = args.get(1).map(|v| v.as_int()).unwrap_or(0).max(0) as usize;
                    let b = args
                        .get(2)
                        .map(|v| v.as_int())
                        .unwrap_or(ch.len() as i32)
                        .max(0) as usize;
                    let a = a.min(ch.len());
                    let b = b.min(ch.len()).max(a);
                    let out: String = ch[a..b].iter().collect();
                    let r = self.new_string(&out);
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
            match name.as_str() {
                "length" => return Ok(Value::Int(me.unwrap_or_default().chars().count() as i32)),
                "isEmpty" => {
                    return Ok(Value::Int(
                        if me.unwrap_or_default().is_empty() { 1 } else { 0 },
                    ))
                }
                "toString" | "intern" | "trim" | "toLowerCase" | "toUpperCase" => {
                    let s = me.unwrap_or_default();
                    let out = match name.as_str() {
                        "trim" => s.trim().to_string(),
                        "toLowerCase" => s.to_lowercase(),
                        "toUpperCase" => s.to_uppercase(),
                        _ => s.clone(),
                    };
                    if out == s {
                        return Ok(args[0].clone());
                    }
                    let r = self.new_string(&out);
                    return Ok(Value::Ref(r));
                }
                "concat" => {
                    let mut s = me.unwrap_or_default();
                    let t = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("")
                        .to_string();
                    s.push_str(&t);
                    let r = self.new_string(&s);
                    return Ok(Value::Ref(r));
                }
                "charAt" => {
                    let i = args.get(1).map(|v| v.as_int()).unwrap_or(0) as usize;
                    let s = me.unwrap_or_default();
                    let ch = s.chars().nth(i).unwrap_or('\0');
                    return Ok(Value::Int(ch as i32));
                }
                "equals" => {
                    let a = me.unwrap_or_default();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("");
                    return Ok(Value::Int(if a == b { 1 } else { 0 }));
                }
                "equalsIgnoreCase" => {
                    let a = me.unwrap_or_default().to_lowercase();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("")
                        .to_lowercase();
                    return Ok(Value::Int(if a == b { 1 } else { 0 }));
                }
                "contains" => {
                    let a = me.unwrap_or_default();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("");
                    return Ok(Value::Int(if a.contains(b) { 1 } else { 0 }));
                }
                "startsWith" => {
                    let a = me.unwrap_or_default();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("");
                    return Ok(Value::Int(if a.starts_with(b) { 1 } else { 0 }));
                }
                "endsWith" => {
                    let a = me.unwrap_or_default();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("");
                    return Ok(Value::Int(if a.ends_with(b) { 1 } else { 0 }));
                }
                "indexOf" => {
                    let a = me.unwrap_or_default();
                    let b = self
                        .string_text(args.get(1).unwrap_or(&Value::Void))
                        .unwrap_or("");
                    let p = a.find(b).map(|x| x as i32).unwrap_or(-1);
                    return Ok(Value::Int(p));
                }
                "hashCode" => {
                    let s = me.unwrap_or_default();
                    let mut h: i32 = 0;
                    for c in s.encode_utf16() {
                        h = h.wrapping_mul(31).wrapping_add(c as i32);
                    }
                    return Ok(Value::Int(h));
                }
                _ => {}
            }
        }

        // Boxing. A boxed value has to carry its own rendering: Java's string
        // concatenation reaches Boolean.valueOf -> toString, and returning the
        // bare int made `false` print as `0` and `true` as `1`.
        if class == "java/lang/Boolean" {
            match name.as_str() {
                "valueOf" => {
                    let b = args.first().map(|v| v.as_int()).unwrap_or(0) != 0;
                    let idx = self.new_holder("Ljava/lang/Boolean;");
                    self.heap[idx as usize].value = Some(Value::Int(i32::from(b)));
                    self.heap[idx as usize].string =
                        Some(if b { "true".into() } else { "false".into() });
                    return Ok(Value::Ref(idx));
                }
                "booleanValue" => {
                    let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
                    let idx = self.ensure_cell(recv, &class);
                    let v = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0));
                    return Ok(Value::Int(i32::from(v.as_int() != 0)));
                }
                "parseBoolean" => {
                    let t = self
                        .string_text(args.first().unwrap_or(&Value::Void))
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    return Ok(Value::Int(i32::from(t == "true")));
                }
                "toString" => {
                    let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
                    let idx = self.ensure_cell(recv, &class);
                    let v = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0));
                    let r = self.new_string(if v.as_int() != 0 { "true" } else { "false" });
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
        }

        if class == "java/lang/Integer" || class == "java/lang/Long" {
            match name.as_str() {
                "parseInt" | "parseLong" => {
                    let s = self
                        .string_text(args.first().unwrap_or(&Value::Void))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    return Ok(Value::Int(s.parse::<i64>().unwrap_or(0) as i32));
                }
                "valueOf" => {
                    let v = args.first().cloned().unwrap_or(Value::Int(0));
                    return Ok(v);
                }
                "toString" => {
                    let v = args.first().cloned().unwrap_or(Value::Int(0));
                    let s = v.as_int().to_string();
                    let r = self.new_string(&s);
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
        }

        if class == "java/lang/StringBuilder" {
            let idx = args.first().map(|v| v.as_ref()).unwrap_or(0);
            let cur = self.heap[idx as usize]
                .string
                .clone()
                .unwrap_or_default();
            match name.as_str() {
                "toString" => {
                    let r = self.new_string(&cur);
                    return Ok(Value::Ref(r));
                }
                "length" => return Ok(Value::Int(cur.chars().count() as i32)),
                _ => {
                    let more = if sig.starts_with("append(Z)") {
                        match args.get(1) {
                            Some(v) => (v.as_int() != 0).to_string(),
                            None => "null".into(),
                        }
                    } else {
                        args.get(1)
                            .map(|v| self.describe(v))
                            .unwrap_or_else(|| "null".into())
                    };
                    let mut s = cur;
                    s.push_str(&more);
                    self.heap[idx as usize].string = Some(s);
                    return Ok(args[0].clone());
                }
            }
        }

        if class == "java/lang/Object" {
            match name.as_str() {
                "hashCode" => return Ok(Value::Int(0)),
                "getClass" => {
                    let t = self
                        .obj(args.first().map(|v| v.as_ref()).unwrap_or(0))
                        .map(|o| o.class_idx)
                        .unwrap_or(NO_INDEX);
                    let c = self.new_class_object(t);
                    return Ok(Value::Ref(c));
                }
                "toString" => {
                    let t = self
                        .obj(args.first().map(|v| v.as_ref()).unwrap_or(0))
                        .map(|o| self.d().class_name(o.class_idx))
                        .unwrap_or_default();
                    let r = self.new_string(&t);
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
        }

        // java.util collections. Backed by `elems`; maps store key/value
        // pairs consecutively so iteration order is insertion order.
        if class.starts_with("java/util/") {
            let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
            let a1 = args.get(1).cloned().unwrap_or(Value::Void);
            let is_set = class.contains("/Set") || class.contains("HashSet");
            let is_map = class.contains("/Map") || class.contains("HashMap");

            // Constructors take no meaningful action for our purposes; the
            // payload is initialised lazily by ensure_holder.
            if name == "<init>" {
                let idx = self.ensure_holder(recv, &class);
                let _ = idx;
                return Ok(Value::Void);
            }

            match (name.as_str(), is_map) {
                ("add", false) => {
                    let idx = self.ensure_holder(recv, &class);
                    let mut added = true;
                    if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                        if is_set {
                            // Sets reject duplicates; compare by identity.
                            added = !e.contains(&a1);
                            if added {
                                e.push(a1);
                            }
                        } else {
                            e.push(a1);
                        }
                    }
                    return Ok(Value::Int(i32::from(added)));
                }
                ("get", true) => {
                    let idx = self.ensure_holder(recv, &class);
                    if let Some(e) = self.heap[idx as usize].elems.as_ref() {
                        let mut i = 0;
                        while i + 1 < e.len() {
                            if e[i] == a1 {
                                return Ok(e[i + 1].clone());
                            }
                            i += 2;
                        }
                    }
                    return Ok(Value::Ref(0));
                }
                ("put", true) => {
                    let idx = self.ensure_holder(recv, &class);
                    let a2 = args.get(2).cloned().unwrap_or(Value::Void);
                    if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                        let mut i = 0;
                        while i + 1 < e.len() {
                            if e[i] == a1 {
                                let old = std::mem::replace(&mut e[i + 1], a2);
                                return Ok(old);
                            }
                            i += 2;
                        }
                        e.push(a1);
                        e.push(a2);
                    }
                    return Ok(Value::Ref(0));
                }
                ("contains", _) | ("containsKey", _) | ("containsValue", _) => {
                    let idx = self.ensure_holder(recv, &class);
                    let hit = self.heap[idx as usize]
                        .elems
                        .as_ref()
                        .map(|e| e.contains(&a1))
                        .unwrap_or(false);
                    return Ok(Value::Int(i32::from(hit)));
                }
                ("remove", _) => {
                    let idx = self.ensure_holder(recv, &class);
                    let mut removed = Value::Ref(0);
                    if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                        if let Some(p) = e.iter().position(|x| *x == a1) {
                            removed = e.remove(p);
                        }
                    }
                    return Ok(removed);
                }
                ("getOrDefault", true) => {
                    let idx = self.ensure_holder(recv, &class);
                    if let Some(e) = self.heap[idx as usize].elems.as_ref() {
                        let mut i = 0;
                        while i + 1 < e.len() {
                            if e[i] == a1 {
                                return Ok(e[i + 1].clone());
                            }
                            i += 2;
                        }
                    }
                    return Ok(args.get(2).cloned().unwrap_or(Value::Void));
                }
                ("keySet", true) | ("values", true) => {
                    let idx = self.new_holder("Ljava/util/HashSet;");
                    let pairs: Vec<Value> = self.heap[recv as usize]
                        .elems
                        .clone()
                        .unwrap_or_default();
                    let step = 2;
                    let skip = usize::from(name == "values");
                    let mut out = Vec::new();
                    let mut i = skip;
                    while i < pairs.len() {
                        out.push(pairs[i].clone());
                        i += step;
                    }
                    self.heap[idx as usize].elems = Some(out);
                    return Ok(Value::Ref(idx));
                }
                ("entrySet", true) => {
                    return Ok(Value::Ref(0));
                }
                ("putAll", true) => {
                    // Copy every pair from another map into this one.
                    let src = args.get(1).map(|v| v.as_ref()).unwrap_or(0);
                    let pairs = self.obj(src).and_then(|o| o.elems.clone()).unwrap_or_default();
                    let idx = self.ensure_holder(recv, &class);
                    if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                        let mut i = 0;
                        while i + 1 < pairs.len() {
                            if !e.iter().any(|x| *x == pairs[i]) {
                                e.push(pairs[i].clone());
                                e.push(pairs[i + 1].clone());
                            }
                            i += 2;
                        }
                    }
                    return Ok(Value::Void);
                }
                ("iterator", _) => {
                    // Snapshot the elements; `value` then holds the cursor.
                    let items = self.obj(recv).and_then(|o| o.elems.clone()).unwrap_or_default();
                    let it = self.new_cell("Ljava/util/ArrayList$Itr;", Value::Int(0));
                    self.heap[it as usize].elems = Some(items);
                    return Ok(Value::Ref(it));
                }
                ("hasNext", _) if class.contains("Iterator") || class.contains("Itr") => {
                    let idx = self.ensure_cell(recv, &class);
                    let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0)).as_int();
                    let n = self.heap[idx as usize].elems.as_ref().map(|e| e.len()).unwrap_or(0);
                    return Ok(Value::Int(i32::from((cur as usize) < n)));
                }
                ("next", _) if class.contains("Iterator") || class.contains("Itr") => {
                    let idx = self.ensure_cell(recv, &class);
                    let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Int(0)).as_int();
                    let got = self.heap[idx as usize]
                        .elems
                        .as_ref()
                        .and_then(|e| e.get(cur as usize).cloned());
                    self.heap[idx as usize].value = Some(Value::Int(cur + 1));
                    return Ok(got.unwrap_or(Value::Ref(0)));
                }
                ("isEmpty", _) => {
                    let idx = self.ensure_holder(recv, &class);
                    let n = self.heap[idx as usize].elems.as_ref().map(|e| e.len()).unwrap_or(0);
                    return Ok(Value::Int(i32::from(n == 0)));
                }
                ("clear", _) => {
                    let idx = self.ensure_holder(recv, &class);
                    if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                        e.clear();
                    }
                    return Ok(Value::Void);
                }
                _ => {}
            }

            // Size differs: maps count entries, sequences count elements.
            if name == "size" {
                let idx = self.ensure_holder(recv, &class);
                let n = self.heap[idx as usize].elems.as_ref().map(|e| e.len()).unwrap_or(0);
                return Ok(Value::Int(if is_map { (n / 2) as i32 } else { n as i32 }));
            }
            // Index access (List / arrays wrapped as sequences).
            if name == "get" && !is_map {
                let idx = self.ensure_holder(recv, &class);
                let i = a1.as_int().max(0) as usize;
                return Ok(self.heap[idx as usize]
                    .elems
                    .as_ref()
                    .and_then(|e| e.get(i).cloned())
                    .unwrap_or(Value::Ref(0)));
            }
        }

        // Boxed primitives and atomics keep their payload in `value`.
        // AtomicReference is a one-slot cell that other threads compare
        // against, and Kotlin's lock acquisition is a compare-and-set retry
        // loop. Returning false here -- which is what happened when the real
        // DEX bytecode ran and hit an Unsafe call we do not have -- means a
        // lock can never be taken, and the app spins in the retry loop
        // forever instead of failing.
        // State.getValue(). Every MutableState keeps its value in a field
        // called `value`; reading it directly is both simpler and more correct
        // than reimplementing the snapshot machinery.
        // android.view.View and its relatives. There is no window and nothing
        // to draw, but these are called by AppCompat and EdgeToEdge on every
        // Activity's way up, and returning null there sends startup down
        // failure paths that have nothing to do with what we are testing.
        // The objects are inert and consistent: one Window per Activity, one
        // decor View under it, and a child list that behaves like a list.
        if class == "android/view/Window" {
            match name.as_str() {
                "getDecorView" => {
                    let r = args.first().map(|v| v.as_ref()).unwrap_or(0);
                    let idx = self.ensure_holder(r, &class);
                    if let Some(e) = self.heap[idx as usize].elems.as_ref() {
                        if let Some(first) = e.first() {
                            return Ok(first.clone());
                        }
                    }
                    let v = self.new_holder("Landroid/view/View;");
                    self.heap[idx as usize].elems = Some(vec![Value::Ref(v)]);
                    return Ok(Value::Ref(v));
                }
                "addFlags" | "clearFlags" | "setStatusBarColor" | "setNavigationBarColor"
                | "setStatusBarColorLongLived" | "setDecorFitsSystemWindows"
                | "setNavigationBarContrastEnforced" => return Ok(Value::Void),
                "getAttributes" => {
                    return Ok(self.service("Landroid/view/WindowManager$LayoutParams;"));
                }
                _ => return Ok(Value::Void),
            }
        }

        if class == "android/view/ViewGroup" || class == "android/view/View" {
            let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
            let idx = self.ensure_holder(recv, &class);
            if name == "addView" {
                // addView(View) and addView(View, LayoutParams)
                let child = args.get(1).cloned().unwrap_or(Value::Ref(0));
                self.heap[idx as usize]
                    .elems
                    .get_or_insert_with(Vec::new)
                    .push(child);
                return Ok(Value::Void);
            }
            if name == "removeView" {
                let child = args.get(1).map(|v| v.as_ref()).unwrap_or(0);
                if let Some(e) = self.heap[idx as usize].elems.as_mut() {
                    e.retain(|v| v.as_ref() != child);
                }
                return Ok(Value::Void);
            }
            return match name.as_str() {
                "getChildCount" => Ok(Value::Int(
                    self.heap[idx as usize].elems.as_ref().map(|e| e.len()).unwrap_or(0) as i32,
                )),
                "getChildAt" => Ok(self.heap[idx as usize]
                    .elems
                    .as_ref()
                    .and_then(|e| e.get(args.get(1).map(|v| v.as_int()).unwrap_or(0) as usize))
                    .cloned()
                    .unwrap_or(Value::Ref(0))),
                "getContext" => Ok(self.application()),
                "getResources" => Ok(self.service("Landroid/content/res/Resources;")),
                "getId" => Ok(Value::Int(0)),
                _ => Ok(Value::Void),
            };
        }

        if class.ends_with("State") && name == "getValue" {
            let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
            if let Some(o) = self.heap.get(recv as usize) {
                let (od, ci) = (o.dex, o.class_idx);
                if let Some(fi) = self.field_index_in(od, ci, "value") {
                    return Ok(self.heap[recv as usize]
                        .fields
                        .get(&fi)
                        .cloned()
                        .unwrap_or(Value::Ref(0)));
                }
            }
            return Ok(args.first().map(|v| v.as_ref()).map(Value::Ref).unwrap_or(Value::Ref(0)));
        }

        if class.contains("AtomicReference") {
            let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
            let idx = self.ensure_cell(recv, &class);
            if self.heap[idx as usize].value.is_none() {
                self.heap[idx as usize].value = Some(Value::Ref(0));
            }
            let cur = self.heap[idx as usize].value.clone().unwrap_or(Value::Ref(0));
            match name.as_str() {
                "get" => return Ok(cur),
                "set" | "lazySet" => {
                    self.heap[idx as usize].value = args.get(1).cloned();
                    return Ok(Value::Void);
                }
                "getAndSet" => {
                    self.heap[idx as usize].value = args.get(1).cloned();
                    return Ok(cur);
                }
                "compareAndSet" | "weakCompareAndSet" => {
                    let want = args.get(1).cloned().unwrap_or(Value::Ref(0));
                    if cur == want {
                        self.heap[idx as usize].value = args.get(2).cloned();
                        return Ok(Value::Int(1));
                    }
                    return Ok(Value::Int(0));
                }
                "toString" => {
                    let r = self.new_string(&self.describe(&cur));
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
        }

        if class.contains("AtomicInteger") || class.contains("/Integer;") {
            let recv = args.first().map(|v| v.as_ref()).unwrap_or(0);
            match name.as_str() {
                "get" | "getAndIncrement" | "incrementAndGet" | "getAndAdd" | "addAndGet"
                | "getAndSet" | "getAndDecrement" | "decrementAndGet" => {
                    let idx = self.ensure_cell(recv, &class);
                    let cur = self.heap[idx as usize]
                        .value
                        .clone()
                        .unwrap_or(Value::Int(0))
                        .as_int();
                    let next = match name.as_str() {
                        "get" => cur,
                        "getAndIncrement" => {
                            self.heap[idx as usize].value = Some(Value::Int(cur + 1));
                            cur
                        }
                        "incrementAndGet" => {
                            cur + 1
                        }
                        "getAndDecrement" => {
                            self.heap[idx as usize].value = Some(Value::Int(cur - 1));
                            cur
                        }
                        "decrementAndGet" => cur - 1,
                        "getAndAdd" => {
                            let d = args.get(1).map(|v| v.as_int()).unwrap_or(0);
                            self.heap[idx as usize].value = Some(Value::Int(cur + d));
                            cur
                        }
                        "addAndGet" => {
                            let d = args.get(1).map(|v| v.as_int()).unwrap_or(0);
                            cur + d
                        }
                        _ => {
                            let v = args.get(1).cloned().unwrap_or(Value::Int(0));
                            self.heap[idx as usize].value = Some(v.clone());
                            v.as_int()
                        }
                    };
                    return Ok(Value::Int(next));
                }
                "set" | "lazySet" => {
                    let idx = self.ensure_cell(recv, &class);
                    let v = args.get(1).cloned().unwrap_or(Value::Int(0));
                    self.heap[idx as usize].value = Some(v.clone());
                    return Ok(Value::Void);
                }
                "intValue" => {
                    let idx = self.ensure_cell(recv, &class);
                    return Ok(self.heap[idx as usize]
                        .value
                        .clone()
                        .unwrap_or(Value::Int(0)));
                }
                _ => {}
            }
        }

        if class == "java/lang/Enum" && name == "compareTo" {
            // Without the enum's declaration order, ordering by identity hash
            // is at least stable and antisymmetric, which is what callers need.
            let other = args.get(1).map(|v| v.as_ref()).unwrap_or(0);
            let a_id = args.first().map(|v| v.as_ref()).unwrap_or(0);
            return Ok(Value::Int((a_id as i32).cmp(&(other as i32)) as i32));
        }

        // android.app.Activity accessors. An Activity launched this way has a
        // single, empty context, so these hand back inert placeholders rather
        // than null, which would trip the caller's own null checks.
        if class == "android/app/Activity" {
            match name.as_str() {
                "getApplication" => {
                    let a = self.new_holder("Landroid/app/Application;");
                    return Ok(Value::Ref(a));
                }
                "getIntent" => {
                    let i = self.new_holder("Landroid/content/Intent;");
                    return Ok(Value::Ref(i));
                }
                "getPackageName" => {
                    return Ok(Value::Ref(self.new_string("org.fdroid")));
                }
                // ComponentActivity's constructor null-checks the result of
                // this before storing it, so returning null aborts startup
                // with a NullPointerException rather than anything useful.
                // The name is per-class so two Activities do not share one.
                "getLifecycle" | "getViewModelStore" | "getSavedStateRegistryOwner"
                | "getOnBackPressedDispatcher" => {
                    return Ok(self.service(&format!("L{class};$member;")));
                }
                _ => {}
            }
        }

        // AndroidX lifecycle. An Activity must have a Lifecycle before it can
        // register anything; the state machine itself is irrelevant here.
        if class == "androidx/lifecycle/LifecycleOwner" && name == "getLifecycle" {
            let l = self.new_holder("Landroidx/lifecycle/Lifecycle;");
            return Ok(Value::Ref(l));
        }
        if class == "androidx/lifecycle/LifecycleOwner"
            && (name == "addLifecycleObserver" || name == "removeLifecycleObserver")
        {
            return Ok(Value::Void);
        }
        if class == "androidx/lifecycle/Lifecycle" {
            match name.as_str() {
                "getCurrentState" => {
                    let st = self.new_holder("Landroidx/lifecycle/Lifecycle$State;");
                    return Ok(Value::Ref(st));
                }
                "addObserver" | "removeObserver" | "handleLifecycleEvent" => {
                    return Ok(Value::Void);
                }
                _ => {}
            }
        }
        // Lifecycle states are ordered, but the only question an app asks at
        // startup is "am I past CREATED?", which is always true once we let an
        // Activity run at all. Match on the method name and a State-ish class
        // so a resolution difference can't silently answer false.
        if name == "isAtLeast" && class.ends_with("State") {
            return Ok(Value::Int(1));
        }
        if name == "getCurrentState" && class.ends_with("Lifecycle") {
            let st = self.new_holder("Landroidx/lifecycle/Lifecycle$State;");
            return Ok(Value::Ref(st));
        }
        if name == "getLifecycle" && class.ends_with("LifecycleOwner") {
            let l = self.new_holder("Landroidx/lifecycle/Lifecycle;");
            return Ok(Value::Ref(l));
        }

        // Logging is pure I/O with no effect on control flow, so the whole
        // SLF4J surface is a no-op.
        if class == "org/slf4j/Logger" {
            return Ok(Value::Void);
        }
        if class == "org/slf4j/LoggerFactory" && name == "getLogger" {
            let t = self.type_index_for("Lorg/slf4j/Logger;");
            let l = self.alloc(t);
            return Ok(Value::Ref(l));
        }

        // java.lang.Class — `represents` holds the described type.
        if class == "java/lang/Class" {
            let represented = self
                .obj(args.first().map(|v| v.as_ref()).unwrap_or(0))
                .and_then(|o| o.represents);
            match name.as_str() {
                "getName" => {
                    let n = represented
                        .map(|t| self.d().class_name(t).replace('/', "."))
                        .unwrap_or_default();
                    let r = self.new_string(&n);
                    return Ok(Value::Ref(r));
                }
                "getSimpleName" => {
                    let n = represented
                        .map(|t| {
                            let full = self.d().class_name(t).replace('/', ".");
                            full.rsplit('.').next().unwrap_or("").to_string()
                        })
                        .unwrap_or_default();
                    let r = self.new_string(&n);
                    return Ok(Value::Ref(r));
                }
                _ => {}
            }
        }

        if (class == "java/lang/String" || class == "java/lang/StringBuilder")
            && name == "format"
        {
            let fmt = self
                .string_text(args.get(1).unwrap_or(&Value::Void))
                .unwrap_or("")
                .to_string();
            let mut out = String::new();
            let mut chars = fmt.chars().peekable();
            let mut argi = 0usize;
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('%') => out.push('%'),
                    Some(spec) => {
                        let mut width = String::new();
                        let mut s = spec;
                        if s.is_ascii_digit() {
                            width.push(s);
                            s = chars.next().unwrap_or('s');
                        }
                        let v = args.get(2 + argi).cloned().unwrap_or(Value::Void);
                        argi += 1;
                        out.push_str(&self.format_one(s, width.parse::<usize>().unwrap_or(0), &v));
                    }
                    None => out.push('%'),
                }
            }
            let r = self.new_string(&out);
            return Ok(Value::Ref(r));
        }

        // Nothing matched: this is Android framework surface we don't model.
        let sig = self.sig_in(&self.d().methods[mid as usize]);
        self.note_missing(&class.replace('/', "."), &sig);

        // Unknown native: return a type-appropriate zero rather than crash.
        if ret.starts_with('V') {
            Ok(Value::Void)
        } else if ret == "Z" || ret == "I" || ret == "S" || ret == "B" || ret == "C" {
            Ok(Value::Int(0))
        } else if ret == "J" {
            Ok(Value::Long(0))
        } else if ret == "F" {
            Ok(Value::Float(0.0))
        } else if ret == "D" {
            Ok(Value::Double(0.0))
        } else {
            Ok(Value::Ref(0))
        }
    }
}