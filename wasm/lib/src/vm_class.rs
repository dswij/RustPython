use convert;
use js_sys::{SyntaxError, TypeError};
use rustpython_vm::{
    compile,
    pyobject::{PyObjectRef, PyRef},
    VirtualMachine,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use wasm_bindgen::prelude::*;
use wasm_builtins::setup_wasm_builtins;

pub(crate) struct StoredVirtualMachine {
    pub vm: VirtualMachine,
    pub scope: PyObjectRef,
}

impl StoredVirtualMachine {
    fn new(id: String, inject_builtins: bool) -> StoredVirtualMachine {
        let mut vm = VirtualMachine::new();
        let builtin = vm.get_builtin_scope();
        let scope = vm.context().new_scope(Some(builtin));
        if inject_builtins {
            setup_wasm_builtins(&mut vm, &scope);
        }
        vm.wasm_id = Some(id);
        StoredVirtualMachine { vm, scope }
    }
}

// It's fine that it's thread local, since WASM doesn't even have threads yet
thread_local! {
    static STORED_VMS: PyRef<HashMap<String, PyRef<StoredVirtualMachine>>> = Rc::default();
    static ACTIVE_VMS: PyRef<HashMap<String, *mut VirtualMachine>> = Rc::default();
}

#[wasm_bindgen(js_name = vmStore)]
pub struct VMStore;

#[wasm_bindgen(js_class = vmStore)]
impl VMStore {
    pub fn init(id: String, inject_builtins: Option<bool>) -> WASMVirtualMachine {
        STORED_VMS.with(|cell| {
            let mut vms = cell.borrow_mut();
            if !vms.contains_key(&id) {
                let stored_vm =
                    StoredVirtualMachine::new(id.clone(), inject_builtins.unwrap_or(true));
                vms.insert(id.clone(), Rc::new(RefCell::new(stored_vm)));
            }
        });
        WASMVirtualMachine { id }
    }

    pub fn get(id: String) -> JsValue {
        STORED_VMS.with(|cell| {
            let vms = cell.borrow();
            if vms.contains_key(&id) {
                WASMVirtualMachine { id }.into()
            } else {
                JsValue::UNDEFINED
            }
        })
    }

    pub fn destroy(id: String) {
        STORED_VMS.with(|cell| {
            use std::collections::hash_map::Entry;
            match cell.borrow_mut().entry(id) {
                Entry::Occupied(o) => {
                    let (_k, stored_vm) = o.remove_entry();
                    // for f in stored_vm.drop_handlers.iter() {
                    //     f();
                    // }
                    // deallocate the VM
                    drop(stored_vm);
                }
                Entry::Vacant(_v) => {}
            }
        });
    }

    pub fn ids() -> Vec<JsValue> {
        STORED_VMS.with(|cell| cell.borrow().keys().map(|k| k.into()).collect())
    }
}

#[derive(Clone)]
pub(crate) struct AccessibleVM {
    weak: Weak<RefCell<StoredVirtualMachine>>,
    id: String,
}

impl AccessibleVM {
    pub fn from_id(id: String) -> AccessibleVM {
        let weak = STORED_VMS
            .with(|cell| Rc::downgrade(cell.borrow().get(&id).expect("WASM VM to be valid")));
        AccessibleVM { weak, id }
    }

    pub fn from_vm(vm: &VirtualMachine) -> AccessibleVM {
        AccessibleVM::from_id(
            vm.wasm_id
                .clone()
                .expect("VM passed to from_vm to have wasm_id be Some()"),
        )
    }

    pub fn upgrade(&self) -> Option<AccessibleVMPtr> {
        let vm_cell = self.weak.upgrade()?;
        let top_level = match vm_cell.try_borrow_mut() {
            Ok(mut vm) => {
                ACTIVE_VMS.with(|cell| {
                    cell.borrow_mut().insert(self.id.clone(), &mut vm.vm);
                });
                true
            }
            Err(_) => false,
        };
        Some(ACTIVE_VMS.with(|cell| {
            let vms = cell.borrow();
            let ptr = vms.get(&self.id).expect("id to be in ACTIVE_VMS");
            let vm = unsafe { &mut **ptr };
            AccessibleVMPtr {
                id: self.id.clone(),
                top_level,
                inner: vm,
            }
        }))
    }
}

impl From<WASMVirtualMachine> for AccessibleVM {
    fn from(vm: WASMVirtualMachine) -> AccessibleVM {
        AccessibleVM::from_id(vm.id)
    }
}
impl From<&WASMVirtualMachine> for AccessibleVM {
    fn from(vm: &WASMVirtualMachine) -> AccessibleVM {
        AccessibleVM::from_id(vm.id.clone())
    }
}

pub(crate) struct AccessibleVMPtr<'a> {
    id: String,
    top_level: bool,
    inner: &'a mut VirtualMachine,
}

impl std::ops::Deref for AccessibleVMPtr<'_> {
    type Target = VirtualMachine;
    fn deref(&self) -> &VirtualMachine {
        &self.inner
    }
}
impl std::ops::DerefMut for AccessibleVMPtr<'_> {
    fn deref_mut(&mut self) -> &mut VirtualMachine {
        &mut self.inner
    }
}

impl Drop for AccessibleVMPtr<'_> {
    fn drop(&mut self) {
        if self.top_level {
            // remove the (now invalid) pointer from the map
            ACTIVE_VMS.with(|cell| cell.borrow_mut().remove(&self.id));
        }
    }
}

#[wasm_bindgen(js_name = VirtualMachine)]
#[derive(Clone)]
pub struct WASMVirtualMachine {
    pub(crate) id: String,
}

#[wasm_bindgen(js_class = VirtualMachine)]
impl WASMVirtualMachine {
    pub(crate) fn with_unchecked<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StoredVirtualMachine) -> R,
    {
        let stored_vm = STORED_VMS.with(|cell| {
            let mut vms = cell.borrow_mut();
            vms.get_mut(&self.id).unwrap().clone()
        });
        let mut stored_vm = stored_vm.borrow_mut();
        f(&mut stored_vm)
    }

    pub(crate) fn with<F, R>(&self, f: F) -> Result<R, JsValue>
    where
        F: FnOnce(&mut StoredVirtualMachine) -> R,
    {
        self.assert_valid()?;
        Ok(self.with_unchecked(f))
    }

    pub fn valid(&self) -> bool {
        STORED_VMS.with(|cell| cell.borrow().contains_key(&self.id))
    }

    pub fn assert_valid(&self) -> Result<(), JsValue> {
        if self.valid() {
            Ok(())
        } else {
            Err(TypeError::new(
                "Invalid VirtualMachine, this VM was destroyed while this reference was still held",
            )
            .into())
        }
    }

    pub fn destroy(&self) -> Result<(), JsValue> {
        self.assert_valid()?;
        VMStore::destroy(self.id.clone());
        Ok(())
    }

    #[wasm_bindgen(js_name = addToScope)]
    pub fn add_to_scope(&self, name: String, value: JsValue) -> Result<(), JsValue> {
        self.with(
            move |StoredVirtualMachine {
                      ref mut vm,
                      ref mut scope,
                  }| {
                let value = convert::js_to_py(vm, value);
                vm.ctx.set_attr(scope, &name, value);
            },
        )
    }

    fn run(&self, mut source: String, mode: compile::Mode) -> Result<JsValue, JsValue> {
        self.assert_valid()?;
        self.with_unchecked(
            |StoredVirtualMachine {
                 ref mut vm,
                 ref mut scope,
             }| {
                source.push('\n');
                let code =
                    compile::compile(&source, &mode, "<wasm>".to_string(), vm.ctx.code_type())
                        .map_err(|err| {
                            SyntaxError::new(&format!("Error parsing Python code: {}", err))
                        })?;
                let result = vm
                    .run_code_obj(code, scope.clone())
                    .map_err(|err| convert::py_str_err(vm, &err))?;
                Ok(convert::py_to_js(vm, result))
            },
        )
    }

    pub fn exec(&self, source: String) -> Result<JsValue, JsValue> {
        self.run(source, compile::Mode::Exec)
    }

    pub fn eval(&self, source: String) -> Result<JsValue, JsValue> {
        self.run(source, compile::Mode::Eval)
    }
}
