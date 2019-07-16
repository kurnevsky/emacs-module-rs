use std::os;
use std::cell::RefCell;
use std::rc::Rc;
use std::ptr;
use std::sync::{Mutex, RwLock, Arc};

use super::error::{ErrorKind, Result};
use super::{Env, Value};
use super::{FromLisp, IntoLisp, Transfer};
use emacs_module::emacs_value;

impl<'a, 'e: 'a> FromLisp<'e> for Value<'a> {
    #[inline(always)]
    fn from_lisp(value: Value<'e>) -> Result<Value<'a>> {
        Ok(value)
    }
}

impl FromLisp<'_> for i64 {
    fn from_lisp(value: Value<'_>) -> Result<Self> {
        raw_call!(value.env, extract_integer, value.raw)
    }
}

impl FromLisp<'_> for f64 {
    fn from_lisp(value: Value<'_>) -> Result<Self> {
        raw_call!(value.env, extract_float, value.raw)
    }
}

impl FromLisp<'_> for String {
    #[cfg(not(feature = "utf-8-validation"))]
    fn from_lisp(value: Value<'_>) -> Result<Self> {
        let bytes = value.env.string_bytes(value)?;
        // Safety: We trust Emacs to give us valid utf-8 bytes.
        unsafe { Ok(String::from_utf8_unchecked(bytes)) }
    }

    #[cfg(feature = "utf-8-validation")]
    fn from_lisp(value: Value<'_>) -> Result<Self> {
        let bytes = value.env.string_bytes(value)?;
        Ok(String::from_utf8(bytes).unwrap())
    }
}

impl<'e, T: FromLisp<'e>> FromLisp<'e> for Option<T> {
    fn from_lisp(value: Value<'e>) -> Result<Self> {
        if value.is_not_nil() {
            Ok(Some(<T as FromLisp>::from_lisp(value)?))
        } else {
            Ok(None)
        }
    }
}

impl<'a, 'e: 'a, T: Transfer> FromLisp<'e> for &'a T {
    fn from_lisp(value: Value<'e>) -> Result<Self> {
        value.env.get_raw_pointer(value.raw).map(|r| unsafe { &*r })
    }
}

impl<'e> IntoLisp<'e> for Value<'e> {
    #[inline(always)]
    fn into_lisp(self, _: &'e Env) -> Result<Value<'_>> {
        Ok(self)
    }
}

impl IntoLisp<'_> for () {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        env.intern("nil")
    }
}

impl IntoLisp<'_> for bool {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        if self {
            env.intern("t")
        } else {
            env.intern("nil")
        }
    }
}

impl IntoLisp<'_> for i64 {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        raw_call_value!(env, make_integer, self)
    }
}

impl IntoLisp<'_> for f64 {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        raw_call_value!(env, make_float, self)
    }
}

impl<'e, 'a, T: AsRef<str> + ?Sized> IntoLisp<'e> for &'a T {
    fn into_lisp(self, env: &'e Env) -> Result<Value<'_>> {
        let bytes = self.as_ref().as_bytes();
        let len = bytes.len();
        let ptr = bytes.as_ptr();
        raw_call_value!(env, make_string, ptr as *const os::raw::c_char, len as isize)
    }
}

impl IntoLisp<'_> for String {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        self.as_str().into_lisp(env)
    }
}

impl<'e, T: IntoLisp<'e>> IntoLisp<'e> for Option<T> {
    fn into_lisp(self, env: &'e Env) -> Result<Value<'_>> {
        match self {
            Some(t) => t.into_lisp(env),
            None => env.intern("nil"),
        }
    }
}

/// Finalizes an embedded pointer. This is called by the GC when it discards a `user-ptr`.
///
/// This function also serves as a form of runtime type tag, relying on Rust's mono-morphization.
unsafe extern "C" fn finalize<T: Transfer>(ptr: *mut os::raw::c_void) {
    #[cfg(build = "debug")]
    println!("Finalizing {} {:#?}", T::type_name(), ptr);
    Box::from_raw(ptr as *mut T);
}

impl<T: Transfer> IntoLisp<'_> for Box<T> {
    fn into_lisp(self, env: &Env) -> Result<Value<'_>> {
        let raw = Box::into_raw(self);
        let ptr = raw as *mut os::raw::c_void;
        raw_call_value!(env, make_user_ptr, Some(finalize::<T>), ptr)
    }
}

enable_transfers! {
    RefCell;
    Mutex;
    RwLock;
    Rc;
    Arc;
}

fn strip_trailing_zero_bytes(bytes: &mut Vec<u8>) {
    let mut len = bytes.len();
    while len > 0 && bytes[len - 1] == 0 {
        bytes.pop(); // strip trailing 0-byte(s)
        len -= 1;
    }
}

type Finalizer = unsafe extern "C" fn(ptr: *mut os::raw::c_void);

/// Implementation details.
impl Env {
    fn string_bytes(&self, value: Value<'_>) -> Result<Vec<u8>> {
        let mut len: isize = 0;
        let mut bytes = unsafe {
            let copy_string_contents = raw_fn!(self, copy_string_contents);
            let ok: bool = self.handle_exit(copy_string_contents(
                self.raw,
                value.raw,
                ptr::null_mut(),
                &mut len,
            ))?;
            // Technically this shouldn't happen, and the return type of copy_string_contents
            // should be void, not bool.
            if !ok {
                panic!("Emacs failed to give string's length but did not raise a signal");
            }

            let mut bytes = vec![0u8; len as usize];
            let ok: bool = self.handle_exit(copy_string_contents(
                self.raw,
                value.raw,
                bytes.as_mut_ptr() as *mut os::raw::c_char,
                &mut len,
            ))?;
            // Technically this shouldn't happen, and the return type of copy_string_contents
            // should be void, not bool.
            if !ok {
                panic!("Emacs failed to copy string but did not raise a signal");
            }
            bytes
        };
        strip_trailing_zero_bytes(&mut bytes);
        Ok(bytes)
    }

    pub(crate) fn get_raw_pointer<T: Transfer>(&self, value: emacs_value) -> Result<*mut T> {
        match raw_call!(self, get_user_finalizer, value)? {
            // TODO: Consider using dynamic dispatch for finalize, and core::any for type checking.
            Some::<Finalizer>(fin) if fin == finalize::<T> => {
                let ptr: *mut os::raw::c_void = raw_call!(self, get_user_ptr, value)?;
                Ok(ptr as *mut T)
            }
            _ => {
                let expected = T::type_name();
                Err(ErrorKind::WrongTypeUserPtr { expected }.into())
            }
        }
    }
}
