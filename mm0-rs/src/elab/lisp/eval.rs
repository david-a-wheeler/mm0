use std::ops::{Deref, DerefMut};
use std::mem;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::collections::{HashMap, hash_map::Entry};
use crate::util::*;
use super::super::{Result, AtomID, FileServer, Elaborator,
  ElabError, ElabErrorKind, ErrorLevel, BoxError};
use super::*;
use super::parser::{IR, Branch, Pattern};

enum Stack<'a> {
  List(Span, Vec<LispVal>, std::slice::Iter<'a, IR>),
  DottedList(Vec<LispVal>, std::slice::Iter<'a, IR>, &'a IR),
  DottedList2(Vec<LispVal>),
  App(Span, Span, &'a [IR]),
  App2(Span, Span, LispVal, Vec<LispVal>, std::slice::Iter<'a, IR>),
  If(&'a IR, &'a IR),
  Def(&'a Option<(Span, AtomID)>),
  Eval(std::slice::Iter<'a, IR>),
  Match(Span, std::slice::Iter<'a, Branch>),
  TestPattern(Span, LispVal, std::slice::Iter<'a, Branch>,
    &'a Branch, Vec<PatternStack<'a>>, Box<[LispVal]>),
  Drop_,
  Ret(FileSpan, ProcPos, Vec<LispVal>, Arc<IR>),
  MatchCont(Span, LispVal, std::slice::Iter<'a, Branch>, Arc<AtomicBool>),
  MapProc(Span, Span, LispVal, Box<[Uncons]>, Vec<LispVal>),
}

impl Stack<'_> {
  fn supports_def(&self) -> bool {
    match self {
      Stack::App2(_, _, _, _, _) => true,
      Stack::Eval(_) => true,
      _ => false,
    }
  }
}
enum State<'a> {
  Eval(&'a IR),
  Ret(LispVal),
  List(Span, Vec<LispVal>, std::slice::Iter<'a, IR>),
  DottedList(Vec<LispVal>, std::slice::Iter<'a, IR>, &'a IR),
  App(Span, Span, LispVal, Vec<LispVal>, std::slice::Iter<'a, IR>),
  Match(Span, LispVal, std::slice::Iter<'a, Branch>),
  Pattern(Span, LispVal, std::slice::Iter<'a, Branch>,
    &'a Branch, Vec<PatternStack<'a>>, Box<[LispVal]>, PatternState<'a>),
  MapProc(Span, Span, LispVal, Box<[Uncons]>, Vec<LispVal>),
}

#[derive(Clone)]
struct Uncons(LispVal, usize);
impl Uncons {
  fn from(e: &LispVal) -> Uncons { Uncons(unwrap(e).into_owned(), 0) }
  fn exactly(&self, n: usize) -> bool {
    match &*self.0 {
      LispKind::List(es) => self.1 + n == es.len(),
      LispKind::DottedList(es, _) if self.1 + n <= es.len() => false,
      LispKind::DottedList(es, r) => Self::from(r).exactly(n - es.len()),
      _ => false,
    }
  }
  fn is_list(&self) -> bool {
    match &*self.0 {
      LispKind::List(_) => true,
      LispKind::DottedList(_, r) => Self::from(r).is_list(),
      _ => false,
    }
  }
  fn at_least(&self, n: usize) -> bool {
    match &*self.0 {
      LispKind::List(es) => return self.1 + n == es.len(),
      LispKind::DottedList(es, r) if self.1 + n <= es.len() => Self::from(r).is_list(),
      LispKind::DottedList(es, r) => Self::from(r).at_least(n - es.len()),
      _ => false,
    }
  }
  fn uncons(&mut self) -> Option<LispVal> {
    loop {
      match &*self.0 {
        LispKind::List(es) => match es.get(self.1) {
          None => return None,
          Some(e) => {self.1 += 1; return Some(e.clone())}
        },
        LispKind::DottedList(es, r) => match es.get(self.1) {
          None => *self = Self::from(r),
          Some(e) => {self.1 += 1; return Some(e.clone())}
        }
        _ => return None
      }
    }
  }
  fn as_lisp(self) -> LispVal {
    if self.1 == 0 {return self.0}
    match &*self.0 {
      LispKind::List(es) if self.1 == es.len() => NIL.clone(),
      LispKind::List(es) => Arc::new(LispKind::List(es[self.1..].into())),
      LispKind::DottedList(es, r) if self.1 == es.len() => r.clone(),
      LispKind::DottedList(es, r) => Arc::new(LispKind::DottedList(es[self.1..].into(), r.clone())),
      _ => unreachable!()
    }
  }
}

enum Dot<'a> { List(Option<usize>), DottedList(&'a Pattern) }
enum PatternStack<'a> {
  List(Uncons, std::slice::Iter<'a, Pattern>, Dot<'a>),
  Binary(bool, bool, LispVal, std::slice::Iter<'a, Pattern>),
}

enum PatternState<'a> {
  Eval(&'a Pattern, LispVal),
  Ret(bool),
  List(Uncons, std::slice::Iter<'a, Pattern>, Dot<'a>),
  Binary(bool, bool, LispVal, std::slice::Iter<'a, Pattern>),
}

struct TestPending(Span, usize);

type SResult<T> = std::result::Result<T, String>;

impl<'a, T: FileServer + ?Sized> Elaborator<'a, T> {
  fn pattern_match<'b>(&mut self, stack: &mut Vec<PatternStack<'b>>, ctx: &mut [LispVal],
      mut active: PatternState<'b>) -> std::result::Result<bool, TestPending> {
    loop {
      active = match active {
        PatternState::Eval(p, e) => match p {
          Pattern::Skip => PatternState::Ret(true),
          &Pattern::Atom(i) => {ctx[i] = e; PatternState::Ret(true)}
          &Pattern::QuoteAtom(a) => PatternState::Ret(
            match &**unwrap(&e) {&LispKind::Atom(a2) => a == a2, _ => false}),
          Pattern::String(s) => PatternState::Ret(
            match &**unwrap(&e) {LispKind::String(s2) => s == s2, _ => false}),
          &Pattern::Bool(b) => PatternState::Ret(
            match &**unwrap(&e) {&LispKind::Bool(b2) => b == b2, _ => false}),
          Pattern::Number(i) => PatternState::Ret(
            match &**unwrap(&e) {LispKind::Number(i2) => i == i2, _ => false}),
          &Pattern::QExprAtom(a) => PatternState::Ret(match &**unwrap(&e) {
            &LispKind::Atom(a2) => a == a2,
            LispKind::List(es) if es.len() == 1 => match &**unwrap(&es[0]) {
              &LispKind::Atom(a2) => a == a2,
              _ => false
            },
            _ => false
          }),
          Pattern::DottedList(ps, r) => PatternState::List(Uncons::from(&e), ps.iter(), Dot::DottedList(r)),
          &Pattern::List(ref ps, n) => PatternState::List(Uncons::from(&e), ps.iter(), Dot::List(n)),
          Pattern::And(ps) => PatternState::Binary(false, false, e, ps.iter()),
          Pattern::Or(ps) => PatternState::Binary(true, true, e, ps.iter()),
          Pattern::Not(ps) => PatternState::Binary(true, false, e, ps.iter()),
          &Pattern::Test(sp, i, ref ps) => {
            stack.push(PatternStack::Binary(false, false, e, ps.iter()));
            return Err(TestPending(sp, i))
          },
        },
        PatternState::Ret(b) => match stack.pop() {
          None => return Ok(b),
          Some(PatternStack::List(u, it, r)) =>
            if b {PatternState::List(u, it, r)}
            else {PatternState::Ret(false)},
          Some(PatternStack::Binary(or, out, u, it)) =>
            if b^or {PatternState::Binary(or, out, u, it)}
            else {PatternState::Ret(out)},
        }
        PatternState::List(mut u, mut it, dot) => match it.next() {
          None => match dot {
            Dot::List(None) => PatternState::Ret(u.exactly(0)),
            Dot::List(Some(n)) => PatternState::Ret(u.at_least(n)),
            Dot::DottedList(p) => PatternState::Eval(p, u.as_lisp()),
          }
          Some(p) => match u.uncons() {
            None => PatternState::Ret(false),
            Some(l) => {
              stack.push(PatternStack::List(u, it, dot));
              PatternState::Eval(p, l)
            }
          }
        },
        PatternState::Binary(or, out, e, mut it) => match it.next() {
          None => PatternState::Ret(!out),
          Some(p) => {
            stack.push(PatternStack::Binary(or, out, e.clone(), it));
            PatternState::Eval(p, e)
          }
        }
      }
    }
  }
}

impl<'a, T: FileServer + ?Sized> Elaborator<'a, T> {
  pub fn print_lisp(&mut self, sp: Span, e: &LispVal) {
    self.errors.push(ElabError::info(sp, format!("{}", self.printer(e))))
  }

  pub fn evaluate<'b>(&'b mut self, ir: &'b IR) -> Result<LispVal> {
    Evaluator::new(self).run(State::Eval(ir))
  }

  pub fn call_func(&mut self, sp: Span, f: LispVal, es: Vec<LispVal>) -> Result<LispVal> {
    Evaluator::new(self).run(State::App(sp, sp, f, es, [].iter()))
  }

  pub fn call_overridable(&mut self, sp: Span, p: BuiltinProc, es: Vec<LispVal>) -> Result<LispVal> {
    let a = self.get_atom(p.to_str());
    let val = match &self.lisp_ctx[a].1 {
      Some((_, e)) => e.clone(),
      None => Arc::new(LispKind::Proc(Proc::Builtin(p)))
    };
    self.call_func(sp, val, es)
  }

  fn as_string(&self, e: &LispVal) -> SResult<ArcString> {
    if let LispKind::String(s) = &**unwrap(e) {Ok(s.clone())} else {
      Err(format!("expected a string, got {}", self.printer(e)))
    }
  }

  fn as_atom_string(&self, e: &LispVal) -> SResult<ArcString> {
    match &**unwrap(e) {
      LispKind::String(s) => Ok(s.clone()),
      &LispKind::Atom(a) => Ok(self.lisp_ctx[a].0.clone()),
      _ => Err(format!("expected an atom, got {}", self.printer(e)))
    }
  }

  fn as_string_atom(&mut self, e: &LispVal) -> SResult<AtomID> {
    match &**unwrap(e) {
      LispKind::String(s) => Ok(self.get_atom(s)),
      &LispKind::Atom(a) => Ok(a),
      _ => Err(format!("expected an atom, got {}", self.printer(e)))
    }
  }

  fn as_int(&self, e: &LispVal) -> SResult<BigInt> {
    if let LispKind::Number(n) = &**unwrap(e) {Ok(n.clone())} else {
      Err(format!("expected a integer, got {}", self.printer(e)))
    }
  }

  fn goal_type(&self, e: &LispVal) -> SResult<LispVal> {
    if let LispKind::Goal(ty) = &**unwrap(e) {Ok(ty.clone())} else {
      Err(format!("expected a integer, got {}", self.printer(e)))
    }
  }

  fn as_ref<'b>(&self, e: &'b LispKind) -> SResult<&'b Mutex<LispVal>> {
    match e {
      LispKind::Ref(m) => Ok(m),
      LispKind::Span(_, e) => self.as_ref(e),
      _ => Err(format!("not a ref-cell: {}", self.printer(e)))
    }
  }

  fn as_map<'b>(&self, e: &'b LispKind) -> SResult<&'b HashMap<AtomID, LispVal>> {
    match e {
      LispKind::AtomMap(m) => Ok(m),
      _ => Err(format!("not an atom map: {}", self.printer(e)))
    }
  }

  fn to_string(&self, e: &LispKind) -> ArcString {
    match e {
      LispKind::Ref(m) => self.to_string(&m.lock().unwrap()),
      LispKind::Span(_, e) => self.to_string(e),
      LispKind::String(s) => s.clone(),
      LispKind::UnparsedFormula(s) => s.clone(),
      LispKind::Atom(a) => self.lisp_ctx[*a].0.clone(),
      LispKind::Number(n) => ArcString::new(n.to_string()),
      _ => ArcString::new(format!("{}", self.printer(e)))
    }
  }

  fn int_bool_binop(&self, mut f: impl FnMut(&BigInt, &BigInt) -> bool, args: &[LispVal]) -> SResult<bool> {
    let mut it = args.iter();
    let mut last = self.as_int(it.next().unwrap())?;
    while let Some(v) = it.next() {
      let new = self.as_int(v)?;
      if !f(&last, &new) {return Ok(false)}
      last = new;
    }
    Ok(true)
  }

  fn head(&self, e: &LispKind) -> SResult<LispVal> {
    match e {
      LispKind::Ref(m) => self.head(&m.lock().unwrap()),
      LispKind::Span(_, e) => self.head(e),
      LispKind::List(es) if es.is_empty() => Err("evaluating 'hd ()'".into()),
      LispKind::List(es) => Ok(es[0].clone()),
      LispKind::DottedList(es, r) if es.is_empty() => self.head(r),
      LispKind::DottedList(es, _) => Ok(es[0].clone()),
      _ => Err(format!("expected a list, got {}", self.printer(e)))
    }
  }

  fn tail(&self, e: &LispKind) -> SResult<LispVal> {
    fn exponential_backoff(es: &[LispVal], i: usize, r: impl FnOnce(Vec<LispVal>) -> LispKind) -> LispVal {
      let j = 2 * i;
      if j >= es.len() { Arc::new(r(es[i..].into())) }
      else { Arc::new(LispKind::DottedList(es[i..j].into(), exponential_backoff(es, j, r))) }
    }
    match e {
      LispKind::Ref(m) => self.tail(&m.lock().unwrap()),
      LispKind::Span(_, e) => self.tail(e),
      LispKind::List(es) if es.is_empty() => Err("evaluating 'tl ()'".into()),
      LispKind::List(es) =>
        Ok(exponential_backoff(es, 1, LispKind::List)),
      LispKind::DottedList(es, r) if es.is_empty() => self.tail(r),
      LispKind::DottedList(es, r) =>
        Ok(exponential_backoff(es, 1, |v| LispKind::DottedList(v, r.clone()))),
      _ => Err(format!("expected a list, got {}", self.printer(e)))
    }
  }

  fn parse_map_insert(&mut self, e: &LispVal) -> SResult<(AtomID, Option<LispVal>)> {
    let mut u = Uncons::from(e);
    let e = u.uncons().ok_or("invalid arguments")?;
    let a = self.as_string_atom(&e)?;
    let ret = u.uncons();
    if !u.exactly(0) {Err("invalid arguments")?}
    Ok((a, ret))
  }
}

struct Evaluator<'a, 'b, T: FileServer + ?Sized> {
  elab: &'b mut Elaborator<'a, T>,
  ctx: Vec<LispVal>,
  file: FileRef,
  stack: Vec<Stack<'b>>,
}
impl<'a, 'b, T: FileServer + ?Sized> Deref for Evaluator<'a, 'b, T> {
  type Target = Elaborator<'a, T>;
  fn deref(&self) -> &Elaborator<'a, T> { self.elab }
}
impl<'a, 'b, T: FileServer + ?Sized> DerefMut for Evaluator<'a, 'b, T> {
  fn deref_mut(&mut self) -> &mut Elaborator<'a, T> { self.elab }
}

impl<'a, 'b, T: FileServer + ?Sized> Evaluator<'a, 'b, T> {
  fn new(elab: &'b mut Elaborator<'a, T>) -> Evaluator<'a, 'b, T> {
    let file = elab.path.clone();
    Evaluator {elab, ctx: vec![], file, stack: vec![]}
  }

  fn make_stack_err(&mut self, sp: Span, level: ErrorLevel,
      mut base: BoxError, err: impl Into<BoxError>) -> ElabError {
    let mut fspan = self.fspan(sp);
    let mut info = vec![];
    for s in self.stack.iter().rev() {
      if let Stack::Ret(_, pos, _, _) = s {
        let (fsp, x) = match pos {
          ProcPos::Named(fsp, a) => (fsp, format!("{}()", self.lisp_ctx[*a].0).into()),
          ProcPos::Unnamed(fsp) => (fsp, "[fn]".into())
        };
        info.push((mem::replace(&mut fspan, fsp.clone()), mem::replace(&mut base, x)))
      }
    }
    ElabError { pos: fspan.span, level, kind: ElabErrorKind::Boxed(err.into(), Some(info)) }
  }

  fn print(&mut self, sp: Span, base: &str, msg: impl Into<BoxError>) {
    let msg = self.make_stack_err(sp, ErrorLevel::Info, base.into(), msg);
    self.errors.push(msg)
  }

  fn err(&mut self, sp: Span, err: impl Into<BoxError>) -> ElabError {
    self.make_stack_err(sp, ErrorLevel::Error, "error occurred here".into(), err)
  }

  fn evaluate_builtin(&mut self, sp1: Span, sp2: Span, f: BuiltinProc, mut args: Vec<LispVal>) -> Result<State<'b>> {
    macro_rules! print {($sp:expr, $e:expr) => {{
      let msg = $e; self.print($sp, f.to_str(), msg)
    }}}
    macro_rules! try1 {($e:expr) => {{
      match $e {
        Ok(e) => e,
        Err(s) => return Err(self.err(sp1, s))
      }
    }}}

    Ok(State::Ret(match f {
      BuiltinProc::Display => {print!(sp1, &*try1!(self.as_string(&args[0]))); UNDEF.clone()}
      BuiltinProc::Error => try1!(Err(&*try1!(self.as_string(&args[0])))),
      BuiltinProc::Print => {print!(sp1, format!("{}", self.printer(&args[0]))); UNDEF.clone()}
      BuiltinProc::Begin => args.last().unwrap_or(&UNDEF).clone(),
      BuiltinProc::Apply => {
        let proc = args.remove(0);
        let sp = proc.fspan().map_or(sp2, |fsp| fsp.span);
        let mut tail = &*args.pop().unwrap();
        loop {match tail {
          LispKind::List(es) => {
            args.extend_from_slice(&es);
            return Ok(State::App(sp1, sp, proc, args, [].iter()))
          }
          LispKind::DottedList(es, r) => {
            args.extend_from_slice(&es);
            tail = r;
          }
          _ => try1!(Err("apply: last argument is not a list"))
        }}
      },
      BuiltinProc::Add => {
        let mut n: BigInt = 0.into();
        for e in args { n += try1!(self.as_int(&e)) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Mul => {
        let mut n: BigInt = 1.into();
        for e in args { n *= try1!(self.as_int(&e)) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Max => {
        let mut n: BigInt = try1!(self.as_int(&args.pop().unwrap())).clone();
        for e in args { n = n.max(try1!(self.as_int(&e)).clone()) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Min => {
        let mut n: BigInt = try1!(self.as_int(&args.pop().unwrap())).clone();
        for e in args { n = n.min(try1!(self.as_int(&e)).clone()) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Sub if args.len() == 1 =>
        Arc::new(LispKind::Number(-try1!(self.as_int(&args[0])).clone())),
      BuiltinProc::Sub => {
        let mut n: BigInt = try1!(self.as_int(&args.pop().unwrap())).clone();
        for e in args { n -= try1!(self.as_int(&e)) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Div => {
        let mut n: BigInt = try1!(self.as_int(&args.pop().unwrap())).clone();
        for e in args { n /= try1!(self.as_int(&e)) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Mod => {
        let mut n: BigInt = try1!(self.as_int(&args.pop().unwrap())).clone();
        for e in args { n %= try1!(self.as_int(&e)) }
        Arc::new(LispKind::Number(n))
      }
      BuiltinProc::Lt => Arc::new(LispKind::Bool(try1!(self.int_bool_binop(|a, b| a < b, &args)))),
      BuiltinProc::Le => Arc::new(LispKind::Bool(try1!(self.int_bool_binop(|a, b| a <= b, &args)))),
      BuiltinProc::Gt => Arc::new(LispKind::Bool(try1!(self.int_bool_binop(|a, b| a > b, &args)))),
      BuiltinProc::Ge => Arc::new(LispKind::Bool(try1!(self.int_bool_binop(|a, b| a >= b, &args)))),
      BuiltinProc::Eq => Arc::new(LispKind::Bool(try1!(self.int_bool_binop(|a, b| a == b, &args)))),
      BuiltinProc::ToString => Arc::new(LispKind::String(self.to_string(&args[0]))),
      BuiltinProc::StringToAtom => {
        let s = try1!(self.as_string(&args[0]));
        Arc::new(LispKind::Atom(self.get_atom(&s)))
      }
      BuiltinProc::StringAppend => {
        let mut out = String::new();
        for e in args { out.push_str(&try1!(self.as_string(&e))) }
        Arc::new(LispKind::String(ArcString::new(out)))
      }
      BuiltinProc::Not => Arc::new(LispKind::Bool(!args.iter().any(|e| e.truthy()))),
      BuiltinProc::And => Arc::new(LispKind::Bool(args.iter().all(|e| e.truthy()))),
      BuiltinProc::Or => Arc::new(LispKind::Bool(args.iter().any(|e| e.truthy()))),
      BuiltinProc::List => Arc::new(LispKind::List(args)),
      BuiltinProc::Cons => match args.len() {
        0 => NIL.clone(),
        1 => args[0].clone(),
        _ => {let r = args.pop().unwrap(); Arc::new(LispKind::DottedList(args, r))}
      },
      BuiltinProc::Head => try1!(self.head(&args[0])),
      BuiltinProc::Tail => try1!(self.tail(&args[0])),
      BuiltinProc::Map => {
        let proc = args[0].clone();
        let sp = proc.fspan().map_or(sp2, |fsp| fsp.span);
        if args.len() == 1 {return Ok(State::App(sp1, sp, proc, vec![], [].iter()))}
        return Ok(State::MapProc(sp1, sp, proc,
          args.into_iter().map(|e| Uncons::from(&e)).collect(), vec![]))
      },
      BuiltinProc::IsBool => Arc::new(LispKind::Bool(args[0].is_bool())),
      BuiltinProc::IsAtom => Arc::new(LispKind::Bool(args[0].is_atom())),
      BuiltinProc::IsPair => Arc::new(LispKind::Bool(args[0].is_pair())),
      BuiltinProc::IsNull => Arc::new(LispKind::Bool(args[0].is_null())),
      BuiltinProc::IsNumber => Arc::new(LispKind::Bool(args[0].is_int())),
      BuiltinProc::IsString => Arc::new(LispKind::Bool(args[0].is_string())),
      BuiltinProc::IsProc => Arc::new(LispKind::Bool(args[0].is_proc())),
      BuiltinProc::IsDef => Arc::new(LispKind::Bool(args[0].is_def())),
      BuiltinProc::IsRef => Arc::new(LispKind::Bool(args[0].is_ref())),
      BuiltinProc::NewRef => Arc::new(LispKind::Ref(Mutex::new(args[0].clone()))),
      BuiltinProc::GetRef => try1!(self.as_ref(&args[0])).lock().unwrap().clone(),
      BuiltinProc::SetRef => {
        *try1!(self.as_ref(&args[0])).lock().unwrap() = args[1].clone();
        UNDEF.clone()
      }
      BuiltinProc::Async => {
        let proc = args.remove(0);
        let sp = proc.fspan().map_or(sp2, |fsp| fsp.span);
        // TODO: actually async this
        return Ok(State::App(sp1, sp, proc, args, [].iter()))
      }
      BuiltinProc::IsAtomMap => Arc::new(LispKind::Bool(args[0].is_map())),
      BuiltinProc::NewAtomMap => {
        let mut m = HashMap::new();
        for e in args {
          match try1!(self.parse_map_insert(&e)) {
            (a, None) => {m.remove(&a);}
            (a, Some(v)) => {m.insert(a, v);}
          }
        }
        Arc::new(LispKind::AtomMap(m))
      }
      BuiltinProc::Lookup => {
        let m = unwrap(&args[0]);
        let m = try1!(self.as_map(&m));
        match m.get(&try1!(self.as_string_atom(&args[1]))) {
          Some(e) => e.clone(),
          None => {
            let v = args.get(2).unwrap_or(&*UNDEF).clone();
            if v.is_proc() {
              let sp = v.fspan().map_or(sp2, |fsp| fsp.span);
              return Ok(State::App(sp1, sp, v, vec![], [].iter()))
            } else {v}
          }
        }
      }
      BuiltinProc::Insert => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::InsertNew => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::SetTimeout => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::IsMVar => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::IsGoal => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::SetMVar => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::PrettyPrint => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::NewGoal => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::GoalType => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::InferType => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::GetMVars => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::GetGoals => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::SetGoals => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::ToExpr => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::Refine => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::Have => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::Stat => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::GetDecl => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::AddDecl => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::AddTerm => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::AddThm => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::SetReporting => {print!(sp2, "unimplemented"); UNDEF.clone()}
      BuiltinProc::RefineExtraArgs => {print!(sp2, "unimplemented"); UNDEF.clone()}
    }))
  }

  fn fspan(&self, span: Span) -> FileSpan {
    FileSpan {file: self.file.clone(), span}
  }

  fn proc_pos(&self, sp: Span) -> ProcPos {
    if let Some(Stack::Def(&Some((sp, x)))) = self.stack.last() {
      ProcPos::Named(self.fspan(sp), x)
    } else {
      ProcPos::Unnamed(self.fspan(sp))
    }
  }

  fn run(&mut self, mut active: State<'b>) -> Result<LispVal> {
    macro_rules! throw {($sp:expr, $e:expr) => {{
      let err = $e;
      return Err(self.err($sp, err))
    }}}
    macro_rules! push {($($e:expr),*; $ret:expr) => {{
      $(self.stack.push({ #[allow(unused_imports)] use Stack::*; $e });)*
      { #[allow(unused_imports)] use State::*; $ret }
    }}}

    loop {
      active = match active {
        State::Eval(ir) => match ir {
          &IR::Local(i) => State::Ret(self.ctx[i].clone()),
          &IR::Global(sp, a) => State::Ret(match &self.lisp_ctx[a] {
            (s, None) => match BuiltinProc::from_str(s) {
              None => throw!(sp, format!("Reference to unbound variable '{}'", s)),
              Some(p) => {
                let s = s.clone();
                let a = self.get_atom(&s);
                let ret = Arc::new(LispKind::Proc(Proc::Builtin(p)));
                self.lisp_ctx[a].1 = Some((None, ret.clone()));
                ret
              }
            },
            (_, Some((_, x))) => x.clone(),
          }),
          IR::Const(val) => State::Ret(val.clone()),
          IR::List(sp, ls) => State::List(*sp, vec![], ls.iter()),
          IR::DottedList(ls, e) => State::DottedList(vec![], ls.iter(), e),
          IR::App(sp1, sp2, f, es) => push!(App(*sp1, *sp2, es); Eval(f)),
          IR::If(e) => push!(If(&e.1, &e.2); Eval(&e.0)),
          IR::Focus(es) => unimplemented!(),
          IR::Def(x, val) => push!(Def(x); Eval(val)),
          IR::Eval(es) => {
            let mut it = es.iter();
            match it.next() {
              None => State::Ret(UNDEF.clone()),
              Some(e) => push!(Eval(it); Eval(e)),
            }
          }
          &IR::Lambda(sp, spec, ref e) =>
            State::Ret(Arc::new(LispKind::Proc(Proc::Lambda {
              pos: self.proc_pos(sp),
              env: self.ctx.clone(),
              spec,
              code: e.clone()
            }))),
          &IR::Match(sp, ref e, ref brs) => push!(Match(sp, brs.iter()); State::Eval(e)),
        },
        State::Ret(ret) => match self.stack.pop() {
          None => return Ok(ret),
          Some(Stack::List(sp, mut vec, it)) => { vec.push(ret); State::List(sp, vec, it) }
          Some(Stack::DottedList(mut vec, it, e)) => { vec.push(ret); State::DottedList(vec, it, e) }
          Some(Stack::DottedList2(vec)) if vec.is_empty() => State::Ret(ret),
          Some(Stack::DottedList2(mut vec)) => State::Ret(Arc::new(match Arc::try_unwrap(ret) {
            Ok(LispKind::List(es)) => { vec.extend(es); LispKind::List(vec) }
            Ok(LispKind::DottedList(es, e)) => { vec.extend(es); LispKind::DottedList(vec, e) }
            Ok(e) => LispKind::DottedList(vec, Arc::new(e)),
            Err(ret) => LispKind::DottedList(vec, ret),
          })),
          Some(Stack::App(sp1, sp2, es)) => State::App(sp1, sp2, ret, vec![], es.iter()),
          Some(Stack::App2(sp1, sp2, f, mut vec, it)) => { vec.push(ret); State::App(sp1, sp2, f, vec, it) }
          Some(Stack::If(e1, e2)) => State::Eval(if unwrap(&ret).truthy() {e1} else {e2}),
          Some(Stack::Def(x)) => {
            match self.stack.pop() {
              None => if let &Some((sp, a)) = x {
                self.lisp_ctx[a].1 = Some((Some(self.fspan(sp)), ret))
              },
              Some(s) if s.supports_def() => push!(Drop_, s; self.ctx.push(ret)),
              Some(s) => self.stack.push(s),
            }
            State::Ret(UNDEF.clone())
          }
          Some(Stack::Eval(mut it)) => match it.next() {
            None => State::Ret(ret),
            Some(e) => push!(Eval(it); Eval(e)),
          },
          Some(Stack::Match(sp, it)) => State::Match(sp, ret, it),
          Some(Stack::TestPattern(sp, e, it, br, pstack, vars)) =>
            State::Pattern(sp, e, it, br, pstack, vars, PatternState::Ret(unwrap(&ret).truthy())),
          Some(Stack::Drop_) => {self.ctx.pop(); State::Ret(ret)}
          Some(Stack::Ret(fsp, _, old, _)) => {self.file = fsp.file; self.ctx = old; State::Ret(ret)}
          Some(Stack::MatchCont(_, _, _, valid)) => {
            if let Err(valid) = Arc::try_unwrap(valid) {valid.store(false, Ordering::Relaxed)}
            State::Ret(ret)
          }
          Some(Stack::MapProc(sp1, sp2, f, us, mut vec)) => {
            vec.push(ret);
            State::MapProc(sp1, sp2, f, us, vec)
          }
        },
        State::List(sp, vec, mut it) => match it.next() {
          None => State::Ret(Arc::new(LispKind::Span(self.fspan(sp),
            Arc::new(LispKind::List(vec))))),
          Some(e) => push!(List(sp, vec, it); Eval(e)),
        },
        State::DottedList(vec, mut it, r) => match it.next() {
          None => push!(DottedList2(vec); Eval(r)),
          Some(e) => push!(DottedList(vec, it, r); Eval(e)),
        },
        State::App(sp1, sp2, f, mut args, mut it) => match it.next() {
          Some(e) => push!(App2(sp1, sp2, f, args, it); Eval(e)),
          None => {
            let f = unwrap(&f);
            let f = match &**f {
              LispKind::Proc(f) => f,
              _ => throw!(sp1, "not a function, cannot apply")
            };
            let spec = f.spec();
            if !spec.valid(args.len()) {
              match spec {
                ProcSpec::Exact(n) => throw!(sp1, format!("expected {} argument(s)", n)),
                ProcSpec::AtLeast(n) => throw!(sp1, format!("expected at least {} argument(s)", n)),
              }
            }
            match f {
              &Proc::Builtin(f) => self.evaluate_builtin(sp1, sp2, f, args)?,
              Proc::Lambda {pos, env, code, ..} => {
                if let Some(Stack::Ret(_, _, _, _)) = self.stack.last() { // tail call
                  if let Some(Stack::Ret(fsp, _, old, _)) = self.stack.pop() {
                    self.ctx = env.clone();
                    self.stack.push(Stack::Ret(fsp, pos.clone(), old, code.clone()));
                  } else {unsafe {std::hint::unreachable_unchecked()}}
                } else {
                  self.stack.push(Stack::Ret(self.fspan(sp1), pos.clone(),
                    mem::replace(&mut self.ctx, env.clone()), code.clone()));
                }
                self.file = pos.fspan().file.clone();
                match spec {
                  ProcSpec::Exact(_) => self.ctx.extend(args),
                  ProcSpec::AtLeast(nargs) => {
                    self.ctx.extend(args.drain(..nargs));
                    self.ctx.push(Arc::new(LispKind::List(args)));
                  }
                }
                // Unfortunately we're fighting the borrow checker here. The problem is that
                // ir is borrowed in the Stack type, with most IR being owned outside the
                // function, but when you apply a lambda, the Proc::LambdaExact constructor
                // stores an Arc to the code to execute, hence it comes under our control,
                // which means that when the temporaries in this block go away, so does
                // ir (which is borrowed from f). We solve the problem by storing an Arc of
                // the IR inside the Ret instruction above, so that it won't get deallocated
                // while in use. Rust doesn't reason about other owners of an Arc though, so...
                State::Eval(unsafe {&*(&**code as *const IR)})
              },
              Proc::MatchCont(valid) => {
                if !valid.load(Ordering::Relaxed) {throw!(sp2, "continuation has expired")}
                loop {
                  match self.stack.pop() {
                    Some(Stack::MatchCont(span, expr, it, a)) => {
                      a.store(false, Ordering::Relaxed);
                      if Arc::ptr_eq(&a, &valid) {
                        break State::Match(span, expr, it)
                      }
                    }
                    Some(Stack::Drop_) => {self.ctx.pop();}
                    Some(Stack::Ret(fsp, _, old, _)) => {self.file = fsp.file; self.ctx = old},
                    Some(_) => {}
                    None => throw!(sp2, "continuation has expired")
                  }
                }
              }
            }
          },
        }
        State::Match(sp, e, mut it) => match it.next() {
          None => throw!(sp, "match failed"),
          Some(br) =>
            State::Pattern(sp, e.clone(), it, br, vec![], vec![UNDEF.clone(); br.vars].into(),
              PatternState::Eval(&br.pat, e))
        },
        State::Pattern(sp, e, it, br, mut pstack, mut vars, st) => {
          match self.pattern_match(&mut pstack, &mut vars, st) {
            Err(TestPending(sp, i)) => push!(
              TestPattern(sp, e.clone(), it, br, pstack, vars);
              App(sp, sp, self.ctx[i].clone(), vec![e], [].iter())),
            Ok(false) => State::Match(sp, e, it),
            Ok(true) => {
              self.ctx.extend_from_slice(&vars);
              if br.cont {
                let valid = Arc::new(AtomicBool::new(true));
                self.ctx.push(Arc::new(LispKind::Proc(Proc::MatchCont(valid.clone()))));
                self.stack.push(Stack::MatchCont(sp, e.clone(), it, valid));
                self.stack.push(Stack::Drop_);
              }
              self.stack.resize_with(self.stack.len() + vars.len(), || Stack::Drop_);
              State::Eval(&br.eval)
            },
          }
        }
        State::MapProc(sp1, sp2, f, mut us, vec) => {
          let mut it = us.iter_mut();
          let u0 = it.next().unwrap();
          match u0.uncons() {
            None => {
              if !(u0.exactly(0) && it.all(|u| u.exactly(0))) {
                throw!(sp1, "mismatched input length")
              }
              State::Ret(Arc::new(LispKind::List(vec)))
            }
            Some(e0) => {
              let mut args = vec![e0];
              for u in it {
                if let Some(e) = u.uncons() {args.push(e)}
                else {throw!(sp1, "mismatched input length")}
              }
              push!(MapProc(sp1, sp2, f.clone(), us, vec); App(sp1, sp2, f, args, [].iter()))
            }
          }
        }
      }
    }
  }
}