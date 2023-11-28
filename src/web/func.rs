use anyhow::Context;
use js_sys::{Array, Function};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};

use crate::{
    backend::{AsContext, AsContextMut, Value, WasmFunc},
    web::{DropResource, JsErrorMsg},
    FuncType,
};

use super::{conversion::ToStoredJs, Engine, StoreContextMut, StoreInner};

/// A bound function
#[derive(Debug, Clone)]
pub struct Func {
    pub(crate) id: usize,
}

/// Internal represenation of [`Func`]
#[derive(Debug)]
pub(crate) struct FuncInner {
    pub(crate) func: Function,
    ty: FuncType,
    name: String,
}

impl ToStoredJs for Func {
    type Repr = Function;
    fn to_stored_js<T>(&self, store: &StoreInner<T>) -> Function {
        let func = &store.funcs[self.id];
        func.func.clone()
    }
}

impl Func {
    pub fn from_exported_function<T>(
        name: &str,
        store: &mut StoreInner<T>,
        value: JsValue,
        signature: FuncType,
    ) -> Option<Self> {
        let func: Function = value.dyn_into().ok()?;

        Some(store.insert_func(FuncInner {
            func,
            // TODO: we don't really know what the exported function's signature is
            ty: signature,
            name: format!("guest.{name}"),
        }))
    }
}

macro_rules! to_ty {
    ($v: ident) => {
        JsValue
    };
}

macro_rules! func_wrapper {
    ($store: ident, $func_ty: ident, $func: ident, $($idx: tt => $ident: ident),*) => {{
        let ty = $func_ty.clone();
        let closure: Closure<dyn FnMut($(to_ty!($ident)),*) -> JsValue> = Closure::new(move |$($ident: JsValue),*| {
            // Safety:
            //
            // This closure is stored inside the store.
            //
            // The closure itself is accessed through a raw pointer, and does not produce any
            // reference to `StoreInner<T>`.
            let store: &mut StoreInner<T> = unsafe { &mut *($store as *mut StoreInner<T>) };
            #[allow(unused_mut)]
            let mut store = StoreContextMut::from_ref(store);

            let (arg_types, ret_types) = ty.params_results.split_at(ty.len_params);

            let args = [
                $(
                    (Value::from_js_typed(&mut store, &arg_types[$idx], $ident)).expect("Failed to convert argument"),
                )*
            ];

            match $func(store, &args) {
                Ok(v) => { v.into() }
                Err(err) => {
                    tracing::error!("{err:?}");
                    return JsValue::UNDEFINED;
                }
            }
        });

        let func = closure.as_ref().unchecked_ref::<Function>().clone();
        let drop_resource = DropResource::new(closure);

        (drop_resource, func)
    }};
}

impl WasmFunc<Engine> for Func {
    fn new<T>(
        name: &str,
        mut ctx: impl AsContextMut<Engine, UserState = T>,
        ty: FuncType,
        func: impl 'static
            + Send
            + Sync
            + Fn(StoreContextMut<T>, &[Value<Engine>], &mut [Value<Engine>]) -> anyhow::Result<()>,
    ) -> Self {
        let name = name.to_string();

        let _span = tracing::debug_span!("Func::new").entered();

        let mut ctx: StoreContextMut<_> = ctx.as_context_mut();

        // Keep a reference to the store to calling context to reconstruct it later during export
        // call.
        //
        // The allocated closure which uses this pointer is stored in the store itself, and as such
        // dropping the store will also drop and prevent any further use of this pointer.
        //
        // The pointer itself is allocated on the stack and will *never* be moved during the entire
        // lifetime of the store.
        //
        // See: [`crate::web::store::Store`] for more details of why this is done this way.
        let store_ptr = ctx.as_ptr();

        // Remove `T` from this pointer and untie the lifetime.
        //
        // Lifetime is enforced by the closured storage in the store, and Store is guaranteed to
        // live as long as this closure
        let store_ptr = store_ptr as *mut ();

        // let mut host_args_ret = vec![Value::I32(0); ty.params_results.len()];

        let mut res = vec![Value::I32(0); ty.results().len()];

        let mut func = {
            let name = name.clone();

            move |mut store: StoreContextMut<T>, args: &[Value<Engine>]| {
                let span = tracing::debug_span!("call_host", name, ?args);
                span.in_scope(|| match func(store.as_context_mut(), args, &mut res) {
                    Ok(v) => {
                        tracing::debug!(?res, "result");
                        Ok(v)
                    }
                    Err(err) => {
                        tracing::error!("{err:?}");
                        Err(err)
                    }
                })?;

                let results = res
                    .iter()
                    .map(|v| v.to_stored_js(&&*store))
                    .collect::<Array>();

                anyhow::Ok(results)
            }
        };

        let (resource, func) = match ty.len_params {
            0 => func_wrapper!(store_ptr, ty, func,),
            1 => func_wrapper!(store_ptr, ty, func, 0 => a),
            2 => func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b),
            3 => func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c),
            4 => func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c, 3 => d),
            5 => func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c, 3 => d, 4 => e),
            6 => func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c, 3 => d, 4 => e, 5 => f),
            7 => {
                func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c, 3 => d, 4 => e, 5 => f, 6 => g)
            }
            8 => {
                func_wrapper!(store_ptr, ty, func, 0 => a, 1 => b, 2 => c, 3 => d, 4 => e, 5 => f, 6 => g, 7 => h)
            }
            v => {
                unimplemented!("exported functions of {v} arguments are not supported")
            }
        };

        let func = ctx.insert_func(FuncInner { func, ty, name });

        tracing::debug!(id = func.id, "func");
        ctx.insert_drop_resource(DropResource::new(resource));

        func
    }

    fn ty(&self, ctx: impl AsContext<Engine>) -> FuncType {
        ctx.as_context().funcs[self.id].ty.clone()
    }

    fn call<T>(
        &self,
        mut ctx: impl AsContextMut<Engine>,
        args: &[Value<Engine>],
        results: &mut [Value<Engine>],
    ) -> anyhow::Result<()> {
        let mut ctx: &mut StoreInner<_> = &mut *ctx.as_context_mut();
        let inner: &FuncInner = &ctx.funcs[self.id];

        let _span = tracing::debug_span!("call_guest", ?args, name = inner.name).entered();

        let args = args.iter().map(|v| v.to_stored_js(&ctx)).collect::<Array>();

        let res = inner
            .func
            .apply(&JsValue::UNDEFINED, &args)
            .map_err(JsErrorMsg::from)
            .context("Guest function threw an error")?;

        tracing::debug!(?res,ty=?inner.ty);

        // https://webassembly.github.io/spec/js-api/#exported-function-exotic-objects
        assert_eq!(inner.ty.results().len(), results.len());
        match inner.ty.results() {
            // void
            [] => {}
            // single
            &[ty] => {
                results[0] = Value::from_js_typed(&mut ctx, &ty, res)
                    .context("Failed to convert return value")?;
            }
            // multi-value
            _ => {
                tracing::error!("multi-value returns are not supported");
                todo!()
            }
        }

        Ok(())
    }
}
