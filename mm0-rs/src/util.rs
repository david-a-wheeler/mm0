use std::ops::{Deref, DerefMut, Range};
use std::borrow::Borrow;
use std::mem::{self, MaybeUninit};
use std::fmt;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::hash::{Hash, Hasher, BuildHasher};
use std::collections::{HashMap, hash_map::{Entry, OccupiedEntry}};
use lsp_types::Url;

pub type BoxError = Box<dyn Error + Send + Sync>;

pub trait HashMapExt<K, V> {
  fn try_insert(&mut self, k: K, v: V) -> Option<(V, OccupiedEntry<K, V>)>;
}
impl<K: Hash + Eq, V, S: BuildHasher> HashMapExt<K, V> for HashMap<K, V, S> {
  fn try_insert(&mut self, k: K, v: V) -> Option<(V, OccupiedEntry<K, V>)> {
    match self.entry(k) {
      Entry::Vacant(e) => { e.insert(v); None }
      Entry::Occupied(e) => Some((v, e))
    }
  }
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)] pub struct ArcString(pub Arc<String>);

impl Borrow<str> for ArcString {
  fn borrow(&self) -> &str { &*self.0 }
}
impl Deref for ArcString {
  type Target = str;
  fn deref(&self) -> &str { &*self.0 }
}
impl ArcString {
  pub fn new(s: String) -> ArcString { ArcString(Arc::new(s)) }
}
impl fmt::Display for ArcString {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
impl From<&str> for ArcString {
  fn from(s: &str) -> ArcString { ArcString::new(s.to_owned()) }
}

pub struct VecUninit<T>(Vec<MaybeUninit<T>>);

impl<T> VecUninit<T> {
  pub fn new(size: usize) -> Self {
    let mut res = Vec::with_capacity(size);
    unsafe { res.set_len(size) };
    VecUninit(res)
  }

  pub fn set(&mut self, i: usize, val: T) {
    self.0[i] = MaybeUninit::new(val);
  }

  pub unsafe fn assume_init(self) -> Vec<T> {
    mem::transmute(self.0)
  }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Span {
  pub start: usize,
  pub end: usize,
}

impl From<Range<usize>> for Span {
  #[inline] fn from(r: Range<usize>) -> Self { Span {start: r.start, end: r.end} }
}

impl From<usize> for Span {
  #[inline] fn from(n: usize) -> Self { Span {start: n, end: n} }
}

impl From<Span> for Range<usize> {
  #[inline] fn from(s: Span) -> Self { s.start..s.end }
}

impl Deref for Span {
  type Target = Range<usize>;
  fn deref(&self) -> &Range<usize> {
    unsafe { mem::transmute(self) }
  }
}

impl DerefMut for Span {
  fn deref_mut(&mut self) -> &mut Range<usize> {
    unsafe { mem::transmute(self) }
  }
}

impl Iterator for Span {
  type Item = usize;
  fn next(&mut self) -> Option<usize> { self.deref_mut().next() }
}
impl DoubleEndedIterator for Span {
  fn next_back(&mut self) -> Option<usize> { self.deref_mut().next_back() }
}

#[derive(Clone, Debug)]
pub struct FileRef(Arc<(PathBuf, Url)>);
impl FileRef {
  pub fn new(buf: PathBuf) -> FileRef {
    let u = Url::from_file_path(&buf).expect("bad file path");
    FileRef(Arc::new((buf, u)))
  }
  pub fn from_url(url: Url) -> FileRef {
    FileRef(Arc::new((url.to_file_path().expect("bad URL"), url)))
  }
  pub fn path(&self) -> &PathBuf { &self.0 .0 }
  pub fn url(&self) -> &Url { &self.0 .1 }
}
impl PartialEq for FileRef {
  fn eq(&self, other: &Self) -> bool { self.0 == other.0 }
}
impl Eq for FileRef {}

impl Hash for FileRef {
  fn hash<H: Hasher>(&self, state: &mut H) { self.0.hash(state) }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileSpan {
  pub file: FileRef,
  pub span: Span,
}