// Copyright (C) 2017-2019 Baidu, Inc. All Rights Reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
//
//  * Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
//  * Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//  * Neither the name of Baidu, Inc., nor the names of its
//    contributors may be used to endorse or promote products derived
//    from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

extern crate sgx_types;
extern crate sgx_urts;
extern crate dirs;

extern crate nan_preserving_float;
extern crate wabt;

use sgx_types::*;
use sgx_urts::SgxEnclave;

use std::io::{Read, Write, stdout};
use std::{fs, path, env, char, io};


mod wasm_def;

use wasm_def::{RuntimeValue, Error as InterpreterError};
use wabt::script::{Action, Command, CommandKind, ScriptParser, Value};
use std::mem::size_of;

extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate chrome_native_messaging;
#[macro_use]
extern crate serde_json;

static ENCLAVE_FILE: &'static str = "enclave.signed.so";
static ENCLAVE_TOKEN: &'static str = "enclave.token";

static MAXOUTPUT:usize = 4096;

extern {
    fn sgxwasm_init(eid: sgx_enclave_id_t, retval: *mut sgx_status_t) -> sgx_status_t ;
    fn sgxwasm_run_action(eid: sgx_enclave_id_t, retval: *mut sgx_status_t,
                          req_bin : *const u8, req_len: usize,
                          result_bin : *mut u8,
                          result_max_len : usize ) -> sgx_status_t;
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SgxWasmAction {
    Invoke {
        module: Option<String>,
        field: String,
        args: Vec<BoundaryValue>
    },
    Get {
        module: Option<String>,
        field: String,
    },
    LoadModule {
        name: Option<String>,
        module: Vec<u8>,
    },
    TryLoad {
        module: Vec<u8>,
    },
    Register {
        name: Option<String>,
        as_name: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BoundaryValue {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
}

#[derive(Serialize, Deserialize, Debug)]
struct Message {
    value: String
}


fn wabt_runtime_value_to_boundary_value(wabt_rv : &wabt::script::Value) -> BoundaryValue {
    match wabt_rv {
        &wabt::script::Value::I32(wabt_rv) => BoundaryValue::I32(wabt_rv),
        &wabt::script::Value::I64(wabt_rv) => BoundaryValue::I64(wabt_rv),
        &wabt::script::Value::F32(wabt_rv) => BoundaryValue::F32(wabt_rv.to_bits()),
        &wabt::script::Value::F64(wabt_rv) => BoundaryValue::F64(wabt_rv.to_bits()),
    }
}

#[allow(dead_code)]
fn runtime_value_to_boundary_value(rv: RuntimeValue) -> BoundaryValue {
    match rv {
        RuntimeValue::I32(rv) => BoundaryValue::I32(rv),
        RuntimeValue::I64(rv) => BoundaryValue::I64(rv),
        RuntimeValue::F32(rv) => BoundaryValue::F32(rv.to_bits()),
        RuntimeValue::F64(rv) => BoundaryValue::F64(rv.to_bits()),
    }
}

fn boundary_value_to_runtime_value(rv: BoundaryValue) -> RuntimeValue {
    match rv {
        BoundaryValue::I32(bv) => RuntimeValue::I32(bv),
        BoundaryValue::I64(bv) => RuntimeValue::I64(bv),
        BoundaryValue::F32(bv) => RuntimeValue::F32(bv.into()),
        BoundaryValue::F64(bv) => RuntimeValue::F64(bv.into()),
    }
}

pub fn answer_convert(res : Result<Option<BoundaryValue>, InterpreterError>)
                     ->  Result<Option<RuntimeValue>, InterpreterError>
{
    match res {
        Ok(None) => Ok(None),
        Ok(Some(rv)) => Ok(Some(boundary_value_to_runtime_value(rv))),
        Err(x) => Err(x),
    }
}

fn spec_to_runtime_value(value: Value) -> RuntimeValue {
    match value {
        Value::I32(v) => RuntimeValue::I32(v),
        Value::I64(v) => RuntimeValue::I64(v),
        Value::F32(v) => RuntimeValue::F32(v.into()),
        Value::F64(v) => RuntimeValue::F64(v.into()),
    }
}

fn runtime_to_spec_value(value: RuntimeValue) -> Value {
    match value {
        RuntimeValue::I32(v) => Value::I32(v),
        RuntimeValue::I64(v) => Value::I64(v),
        RuntimeValue::F32(v) => Value::F32(v.into()),
        RuntimeValue::F64(v) => Value::F64(v.into()),
    }
}

fn init_enclave() -> SgxResult<SgxEnclave> {

    let mut launch_token: sgx_launch_token_t = [0; 1024];
    let mut launch_token_updated: i32 = 0;
    // Step 1: try to retrieve the launch token saved by last transaction
    //         if there is no token, then create a new one.
    //
    // try to get the token saved in $HOME */
    let mut home_dir = path::PathBuf::new();
    let use_token = match dirs::home_dir() {
        Some(path) => {
            //println!("[+] Home dir is {}", path.display());
            home_dir = path;
            true
        },
        None => {
            //println!("[-] Cannot get home dir");
            false
        }
    };

    let token_file: path::PathBuf = home_dir.join(ENCLAVE_TOKEN);;
    if use_token == true {
        match fs::File::open(&token_file) {
            Err(_) => {
                //println!("[-] Open token file {} error! Will create one.", token_file.as_path().to_str().unwrap());
                launch_token_updated = 1;
            },
            Ok(mut f) => {
                //println!("[+] Open token file success! ");
                match f.read(&mut launch_token) {
                    Ok(1024) => {
                        //println!("[+] Token file valid!");
                    },
                    _ => {
                        //println!("[+] Token file invalid, will create new token file");
                        launch_token_updated = 1;
                    },
                }
            }
        }
    }

    // Step 2: call sgx_create_enclave to initialize an enclave instance
    // Debug Support: set 2nd parameter to 1
    let debug = 1;
    let mut misc_attr = sgx_misc_attribute_t {secs_attr: sgx_attributes_t { flags:0, xfrm:0}, misc_select:0};
    let mut enclave_file_path = std::env::current_exe().unwrap();
    enclave_file_path.pop();
    assert!(env::set_current_dir(&enclave_file_path).is_ok());
    let enclave = try!(SgxEnclave::create(ENCLAVE_FILE,
                                          debug,
                                          &mut launch_token,
                                          &mut launch_token_updated,
                                          &mut misc_attr));

    // Step 3: save the launch token if it is updated
    if use_token == true && launch_token_updated != 0 {
        // reopen the file with write capablity
        match fs::File::create(&token_file) {
            Ok(mut f) => {
                match f.write_all(&launch_token) {
                    Ok(()) => {},
                    Err(_) => {},
                }
            },
            Err(_) => {
                //println!("[-] Failed to save updated enclave token, but doesn't matter");
            },
        }
    }

    Ok(enclave)
}

fn sgx_enclave_wasm_init(enclave : &SgxEnclave) -> Result<(),String> {
    let mut retval:sgx_status_t = sgx_status_t::SGX_SUCCESS;
    let result = unsafe {
        sgxwasm_init(enclave.geteid(),
                     &mut retval)
    };

    match result {
        sgx_status_t::SGX_SUCCESS => {},
        _ => {
            println!("[-] ECALL Enclave Failed {}!", result.as_str());
            panic!("sgx_enclave_wasm_init's ECALL returned unknown error!");
        }
    }

    match retval {
        sgx_status_t::SGX_SUCCESS => {},
        _ => {
            println!("[-] ECALL Enclave Function return fail: {}!", retval.as_str());
            return Err(format!("ECALL func return error: {}", retval.as_str()));
        }
    }

    Ok(())
}

fn sgx_enclave_wasm_invoke(req_str : String,
                           result_max_len : usize,
                           enclave : &SgxEnclave) -> (Result<Option<BoundaryValue>, InterpreterError>, sgx_status_t) {
    let enclave_id = enclave.geteid();
    let mut ret_val = sgx_status_t::SGX_SUCCESS;
    let     req_bin = req_str.as_ptr() as * const u8;
    let     req_len = req_str.len();

    let mut result_vec:Vec<u8> = vec![0; result_max_len];
    let     result_slice = &mut result_vec[..];

    let sgx_ret = unsafe{sgxwasm_run_action(enclave_id,
                                     &mut ret_val,
                                     req_bin,
                                     req_len,
                                     result_slice.as_mut_ptr(),
                                     result_max_len)};

    match sgx_ret {
        // sgx_ret falls in range of Intel's Error code set
        sgx_status_t::SGX_SUCCESS => {},
        _ => {
            println!("[-] ECALL Enclave Failed {}!", sgx_ret.as_str());
            panic!("sgx_enclave_wasm_load_invoke's ECALL returned unknown error!");
        }
    }

    // We need to trim all trailing '\0's before conver to string
    let mut result_vec:Vec<u8> = result_slice.to_vec();
    result_vec.retain(|x| *x != 0x00u8);

    //let result_str : String;
    let result:Result<Option<BoundaryValue>, InterpreterError>;
    // Now result_vec only includes essential chars
    if result_vec.len() == 0 {
        result = Ok(None);
    }
    else{
        let raw_result_str = String::from_utf8(result_vec).unwrap();
        result = serde_json::from_str(&raw_result_str).unwrap();
    }

    match ret_val {
        // ret_val falls in range of [SGX_SUCCESS + SGX_ERROR_WASM_*]
        sgx_status_t::SGX_SUCCESS => {},
        _ => {
            // In this case, the returned buffer is useful
            return (result, ret_val);
        }
    }



    // ret_val should be SGX_SUCCESS here
    (result, ret_val)
}

fn sgx_enclave_wasm_load_module(module : Vec<u8>,
                                name   : &Option<String>,
                                enclave : &SgxEnclave)
                                -> Result<(), String> {

    // Init a SgxWasmAction::LoadModule struct and send it to enclave
    let req = SgxWasmAction::LoadModule {
                  name : name.as_ref().map(|x| x.clone()),
                  module : module,
              };

    match sgx_enclave_wasm_invoke(serde_json::to_string(&req).unwrap(),
                                  MAXOUTPUT,
                                  enclave) {
        (_, sgx_status_t::SGX_SUCCESS) => {
            Ok(())
        },
        (Err(x), sgx_status_t::SGX_ERROR_WASM_LOAD_MODULE_ERROR) => {
            Err(x.to_string())
        },
        (_, _) => {
            println!("sgx_enclave_wasm_load_module should not arrive here!");
            panic!("sgx_enclave_wasm_load_module returned unknown error!");
        },
    }
}


fn sgx_enclave_wasm_run_action(action : &Action, enclave : &SgxEnclave) -> Result<Option<RuntimeValue>, InterpreterError> {
    match action {
        &Action::Invoke {
            ref module,
            ref field,
            ref args,
        } => {
            // Deal with Invoke
            // Make a SgxWasmAction::Invoke structure and send it to sgx_enclave_wasm_invoke
            let req = SgxWasmAction::Invoke {
                          module : module.as_ref().map(|x| x.clone()),
                          field  : field.clone(),
                          args   : args.into_iter()
                                       .map(wabt_runtime_value_to_boundary_value)
                                       .collect()
            };
            let result = sgx_enclave_wasm_invoke(serde_json::to_string(&req).unwrap(),
                                                 MAXOUTPUT,
                                                 enclave);
            match result {
                (result, sgx_status_t::SGX_SUCCESS) => {
                    let result_obj : Result<Option<RuntimeValue>, InterpreterError> = answer_convert(result);
                    result_obj
                },
                (result, sgx_status_t::SGX_ERROR_WASM_INTERPRETER_ERROR) => {
                    let result_obj : Result<Option<RuntimeValue>, InterpreterError> = answer_convert(result);
                    result_obj
                },
                (_, _) => {
                    println!("sgx_enclave_wasm_run_action::Invoke returned unknown error!");
                    panic!("sgx_enclave_wasm_run_action::Invoke returned unknown error!");
                },
            }
        },
        &Action::Get {
            ref module,
            ref field,
            ..
        } => {
            // Deal with Get
            // Make a SgxWasmAction::Get structure and send it to sgx_enclave_wasm_invoke
            let req = SgxWasmAction::Get {
                module : module.as_ref().map(|x| x.clone()),
                field  : field.clone(),
            };
            let result = sgx_enclave_wasm_invoke(serde_json::to_string(&req).unwrap(),
                                                 MAXOUTPUT,
                                                 enclave);

            match result {
                (result, sgx_status_t::SGX_SUCCESS) => {
                    let result_obj : Result<Option<RuntimeValue>, InterpreterError> = answer_convert(result);
                    result_obj
                },
                (result, sgx_status_t::SGX_ERROR_WASM_INTERPRETER_ERROR) => {
                    let result_obj : Result<Option<RuntimeValue>, InterpreterError> = answer_convert(result);
                    result_obj
                },
                (_, _) => { println!("sgx_enclave_wasm_run_action::Get returned unknown error!");
                    panic!("sgx_enclave_wasm_run_action::Get returned unknown error!");
                }
            }
        },
    }
}

// Malform
fn sgx_enclave_wasm_try_load(module : &[u8], enclave : &SgxEnclave) -> Result<(), InterpreterError> {
    // Make a SgxWasmAction::TryLoad structure and send it to sgx_enclave_wasm_invoke
    let req = SgxWasmAction::TryLoad {
        module : module.to_vec(),
    };

    let result = sgx_enclave_wasm_invoke(serde_json::to_string(&req).unwrap(),
                                         MAXOUTPUT,
                                         enclave);
    match result {
        (_, sgx_status_t::SGX_SUCCESS) => {
            Ok(())
        },
        (Err(x), sgx_status_t::SGX_ERROR_WASM_TRY_LOAD_ERROR) => {
            Err(InterpreterError::Global(x.to_string()))
        },
        (_, _) => {
            println!("sgx_enclave_wasm_try_load returned unknown error!");
            panic!("sgx_enclave_wasm_try_load returned unknown error!");
        }
    }
}

// Register
fn sgx_enclave_wasm_register(name : Option<String>,
                             as_name : String,
                             enclave : &SgxEnclave) -> Result<(), InterpreterError> {
    // Make a SgxWasmAction::Register structure and send it to sgx_enclave_wasm_invoke
    let req = SgxWasmAction::Register{
        name : name,
        as_name : as_name,
    };

    let result = sgx_enclave_wasm_invoke(serde_json::to_string(&req).unwrap(),
                                         MAXOUTPUT,
                                         enclave);

    match result {
        (_, sgx_status_t::SGX_SUCCESS) => {
            Ok(())
        },
        (Err(x), sgx_status_t::SGX_ERROR_WASM_REGISTER_ERROR) => {
            Err(InterpreterError::Global(x.to_string()))
        },
        (_, _) => {
            println!("sgx_enclave_wasm_register returned unknown error!");
            panic!("sgx_enclave_wasm_register returned unknown error!");
        }
    }
}

fn wasm_main_loop(wast_file : &str, enclave : &SgxEnclave) -> Result<(), String> {

    // ScriptParser interface has changed. Need to feed it with wast content.
    let mut parser = ScriptParser::from_str(wast_file).unwrap();

    sgx_enclave_wasm_init(enclave)?;
    while let Some(Command{kind,line}) =
            match parser.next() {
                Ok(x) => x,
                _ => { return Err("Error parsing test input".to_string()); }
            }
    {
        //println!("Line : {}", line);

        match kind {
            CommandKind::Module { name, module, .. } => {
                sgx_enclave_wasm_load_module (module.into_vec(), &name, enclave)?;
                //println!("load module - success at line {}", line)
            },

            CommandKind::AssertReturn { action, expected } => {
                let result:Result<Option<RuntimeValue>, InterpreterError> = sgx_enclave_wasm_run_action(&action, enclave);
                match result {
                    Ok(result) => {
                        let spec_expected = expected.iter()
                                                    .cloned()
                                                    .map(spec_to_runtime_value)
                                                    .collect::<Vec<_>>();
                        let actual_result = result.into_iter().collect::<Vec<RuntimeValue>>();
                        for (actual_result, spec_expected) in actual_result.iter().zip(spec_expected.iter()) {
                            assert_eq!(actual_result.value_type(), spec_expected.value_type());
                            // f32::NAN != f32::NAN
                            match spec_expected {
                                &RuntimeValue::F32(val) if val.is_nan() => match actual_result {
                                    &RuntimeValue::F32(val) => assert!(val.is_nan()),
                                    _ => unreachable!(), // checked above that types are same
                                },
                                &RuntimeValue::F64(val) if val.is_nan() => match actual_result {
                                    &RuntimeValue::F64(val) => assert!(val.is_nan()),
                                    _ => unreachable!(), // checked above that types are same
                                },
                                spec_expected @ _ => assert_eq!(actual_result, spec_expected),
                            }
                        }
                        println!("assert_return at line {} - success", line);
                    },
                    Err(e) => {
                        panic!("Expected action to return value, got error: {:?}", e);
                    }
                }
            },

            CommandKind::AssertReturnCanonicalNan { action }
            | CommandKind::AssertReturnArithmeticNan { action } => {
                let result:Result<Option<RuntimeValue>, InterpreterError> = sgx_enclave_wasm_run_action(&action, enclave);
                match result {
                    Ok(result) => {
                        for actual_result in result.into_iter().collect::<Vec<RuntimeValue>>() {
                            match actual_result {
                                RuntimeValue::F32(val) => if !val.is_nan() {
                                    panic!("Expected nan value, got {:?}", val)
                                },
                                RuntimeValue::F64(val) => if !val.is_nan() {
                                    panic!("Expected nan value, got {:?}", val)
                                },
                                val @ _ => {
                                    panic!("Expected action to return float value, got {:?}", val)
                                }
                            }
                        }
                        println!("assert_return_nan at line {} - success", line);
                    }
                    Err(e) => {
                        panic!("Expected action to return value, got error: {:?}", e);
                    }
                }
            },

            CommandKind::AssertExhaustion { action, .. } => {
                let result:Result<Option<RuntimeValue>, InterpreterError> = sgx_enclave_wasm_run_action(&action, enclave);
                match result {
                    Ok(result) => panic!("Expected exhaustion, got result: {:?}", result),
                    Err(e) => println!("assert_exhaustion at line {} - success ({:?})", line, e),
                }
            },

            CommandKind::AssertTrap { action, .. } => {
                println!("Enter AssertTrap!");
                let result:Result<Option<RuntimeValue>, InterpreterError> = sgx_enclave_wasm_run_action(&action, enclave);
                match result {
                    Ok(result) => {
                        panic!("Expected action to result in a trap, got result: {:?}", result);
                    },
                    Err(e) => {
                        println!("assert_trap at line {} - success ({:?})", line, e);
                    },
                }
            },

            CommandKind::AssertInvalid { module, .. }
            | CommandKind::AssertMalformed { module, .. }
            | CommandKind::AssertUnlinkable { module, .. } => {
                // Malformed
                let module_load = sgx_enclave_wasm_try_load(&module.into_vec(), enclave);
                match module_load {
                    Ok(_) => panic!("Expected invalid module definition, got some module!"),
                    Err(e) => println!("assert_invalid at line {} - success ({:?})", line, e),
                }
            },

            CommandKind::AssertUninstantiable { module, .. } => {
                let module_load = sgx_enclave_wasm_try_load(&module.into_vec(), enclave);
                match module_load {
                    Ok(_) => panic!("Expected error running start function at line {}", line),
                    Err(e) => println!("assert_uninstantiable - success ({:?})", e),
                }
            },

            CommandKind::Register { name, as_name, .. } => {
                let result = sgx_enclave_wasm_register(name, as_name, enclave);
                match result {
                    Ok(_) => {println!("register - success at line {}", line)},
                    Err(e) => panic!("No such module, at line {} - ({:?})", e, line),
                }
            },

            CommandKind::PerformAction(action) => {
                let result:Result<Option<RuntimeValue>, InterpreterError> = sgx_enclave_wasm_run_action(&action, enclave);
                match result {
                    Ok(result) => {
                        match result{
                            Some(x) => {
                                match x {
                                    RuntimeValue::I32(v) => {
                                        let x = json!({ "res": v });
                                             chrome_native_messaging::write_output(io::stdout(), &x)
                                                 .expect("failed to write to stdout");
                                    },
                                    RuntimeValue::I64(v) => {
                                        let x = json!({ "res": v });
                                             chrome_native_messaging::write_output(io::stdout(), &x)
                                                 .expect("failed to write to stdout");
                                    },
                                    _ => {}
                                };
                                //let str_to_send = format!("{{val:\"{:?}\"}}", x)
                                //let msg = serde_json::from_str("{value:\"hello\"}").unwrap();
                                //chrome_native_messaging::write_output(io::stdout(),msg);


                            },

                            None => println!("invoke - success at line {}", line),
                        }
                    },
                    Err(e) => panic!("Failed to invoke action at line {}: {:?}", line, e),
                }
            },
        }
    }
    //println!("[+] all tests passed!");
    Ok(())
}

fn run_a_wast(enclave   : &SgxEnclave,
              wast_file : &str) -> Result<(), String> {

    // Step 1: Init the sgxwasm spec driver engine
    sgx_enclave_wasm_init(enclave)?;

    // Step 2: Load the wast file and run
    wasm_main_loop(wast_file, enclave)?;

    Ok(())
}

fn main() {

    let enclave = match init_enclave() {
        Ok(r) => {
            //println!("[+] Init Enclave Successful {}!", r.geteid());
            r
        },
        Err(x) => {
            println!("[-] Init Enclave Failed {}!", x.as_str());
            return;
        },
    };




    let mut input = chrome_native_messaging::read_input(std::io::stdin()).unwrap();


    let mut x = input.get("wasmcode").unwrap();
    let program = x.as_str().unwrap();
    run_a_wast(&enclave, program).unwrap();



    enclave.destroy();
    //println!("[+] run_wasm success...");

    return;
}
