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

#![crate_name = "pcl_seal"]
#![crate_type = "staticlib"]

#![cfg_attr(not(target_env = "sgx"), no_std)]
#![cfg_attr(target_env = "sgx", feature(rustc_private))]

#![warn(unused_extern_crates)]

extern crate sgx_types;
extern crate sgx_tcrypto;
extern crate sgx_tse;
extern crate sgx_tseal;
#[cfg(not(target_env = "sgx"))]
#[macro_use]
extern crate sgx_tstd as std;
extern crate sgx_rand;

extern crate rustls;
extern crate webpki;
extern crate webpki_roots;
extern crate itertools;
extern crate base64;
extern crate httparse;
extern crate yasna;
extern crate bit_vec;
extern crate num_bigint;
extern crate chrono;
extern crate ue_send_recv;

use sgx_types::*;
use sgx_tse::*;
use sgx_tcrypto::*;
use sgx_rand::*;

use std::prelude::v1::*;
use std::sync::Arc;
use std::net::TcpStream;
use std::string::String;
use std::ptr;
use std::str;
use std::sgxfs::SgxFile;
use std::io::{self, Write, Read, BufReader};
use std::untrusted::fs;
use std::vec::Vec;
use itertools::Itertools;
use ue_send_recv::{tls_receive_vec, tls_send_vec};

mod cert;
mod hex;

pub const DEV_HOSTNAME:&'static str = "api.trustedservices.intel.com";
pub const SIGRL_SUFFIX:&'static str = "/sgx/dev/attestation/v3/sigrl/";
pub const REPORT_SUFFIX:&'static str = "/sgx/dev/attestation/v3/report";
pub const CERTEXPIRYDAYS: i64 = 90i64;

pub const KEYFILE:&'static str = "prov_key.bin";
#[allow(dead_code)]
const PCL_SEALED_KEY_SIZE: usize = 560 + SGX_AESGCM_KEY_SIZE + SGX_PCL_GUID_SIZE; // sizeof sgx_sealed_data_t = 560

extern "C" {
    pub fn ocall_sgx_init_quote ( ret_val : *mut sgx_status_t,
                  ret_ti  : *mut sgx_target_info_t,
                  ret_gid : *mut sgx_epid_group_id_t) -> sgx_status_t;
    pub fn ocall_get_ias_socket ( ret_val : *mut sgx_status_t,
                  ret_fd  : *mut i32) -> sgx_status_t;
    pub fn ocall_get_quote (ret_val            : *mut sgx_status_t,
                p_sigrl            : *const u8,
                sigrl_len          : u32,
                p_report           : *const sgx_report_t,
                quote_type         : sgx_quote_sign_type_t,
                p_spid             : *const sgx_spid_t,
                p_nonce            : *const sgx_quote_nonce_t,
                p_qe_report        : *mut sgx_report_t,
                p_quote            : *mut u8,
                maxlen             : u32,
                p_quote_len        : *mut u32) -> sgx_status_t;
}

fn parse_response_attn_report(resp : &[u8]) -> (String, String, String){
    println!("parse_response_attn_report");
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut respp   = httparse::Response::new(&mut headers);
    let result = respp.parse(resp);
    println!("parse result {:?}", result);

    let msg : &'static str;

    match respp.code {
        Some(200) => msg = "OK Operation Successful",
        Some(401) => msg = "Unauthorized Failed to authenticate or authorize request.",
        Some(404) => msg = "Not Found GID does not refer to a valid EPID group ID.",
        Some(500) => msg = "Internal error occurred",
        Some(503) => msg = "Service is currently not able to process the request (due to
            a temporary overloading or maintenance). This is a
            temporary state – the same request can be repeated after
            some time. ",
        _ => {println!("DBG:{}", respp.code.unwrap()); msg = "Unknown error occured"},
    }

    println!("{}", msg);
    let mut len_num : u32 = 0;

    let mut sig = String::new();
    let mut cert = String::new();
    let mut attn_report = String::new();

    for i in 0..respp.headers.len() {
        let h = respp.headers[i];
        //println!("{} : {}", h.name, str::from_utf8(h.value).unwrap());
        match h.name{
            "content-length" => {
                let len_str = String::from_utf8(h.value.to_vec()).unwrap();
                len_num = len_str.parse::<u32>().unwrap();
                println!("content length = {}", len_num);
            }
            "x-iasreport-signature" => sig = str::from_utf8(h.value).unwrap().to_string(),
            "x-iasreport-signing-certificate" => cert = str::from_utf8(h.value).unwrap().to_string(),
            _ => (),
        }
    }

    // Remove %0A from cert, and only obtain the signing cert
    cert = cert.replace("%0A", "");
    cert = cert::percent_decode(cert);
    let v: Vec<&str> = cert.split("-----").collect();
    let sig_cert = v[2].to_string();

    if len_num != 0 {
        let header_len = result.unwrap().unwrap();
        let resp_body = &resp[header_len..];
        attn_report = str::from_utf8(resp_body).unwrap().to_string();
        println!("Attestation report: {}", attn_report);
    }

    // len_num == 0
    (attn_report, sig, sig_cert)
}


fn parse_response_sigrl(resp : &[u8]) -> Vec<u8> {
    println!("parse_response_sigrl");
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut respp   = httparse::Response::new(&mut headers);
    let result = respp.parse(resp);
    println!("parse result {:?}", result);
    println!("parse response{:?}", respp);

    let msg : &'static str;

    match respp.code {
        Some(200) => msg = "OK Operation Successful",
        Some(401) => msg = "Unauthorized Failed to authenticate or authorize request.",
        Some(404) => msg = "Not Found GID does not refer to a valid EPID group ID.",
        Some(500) => msg = "Internal error occurred",
        Some(503) => msg = "Service is currently not able to process the request (due to
            a temporary overloading or maintenance). This is a
            temporary state – the same request can be repeated after
            some time. ",
        _ => msg = "Unknown error occured",
    }

    println!("{}", msg);
    let mut len_num : u32 = 0;

    for i in 0..respp.headers.len() {
        let h = respp.headers[i];
        if h.name == "content-length" {
            let len_str = String::from_utf8(h.value.to_vec()).unwrap();
            len_num = len_str.parse::<u32>().unwrap();
            println!("content length = {}", len_num);
        }
    }

    if len_num != 0 {
        let header_len = result.unwrap().unwrap();
        let resp_body = &resp[header_len..];
        println!("Base64-encoded SigRL: {:?}", resp_body);

        return base64::decode(str::from_utf8(resp_body).unwrap()).unwrap();
    }

    // len_num == 0
    Vec::new()
}

pub fn make_ias_client_config() -> rustls::ClientConfig {
    let mut config = rustls::ClientConfig::new();

    config.root_store.add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);

    let certs = load_certs("client.crt");
    let privkey = load_private_key("client.key");
    config.set_single_client_cert(certs, privkey);

    config
}


pub fn get_sigrl_from_intel(fd : c_int, gid : u32) -> Vec<u8> {
    println!("get_sigrl_from_intel fd = {:?}", fd);
    let config = make_ias_client_config();

    let req = format!("GET {}{:08x} HTTP/1.1\r\nHOST: {}\r\n\r\n",
                      SIGRL_SUFFIX,
                      gid,
                      SIGRL_SUFFIX);
    println!("{}", req);

    let dns_name = webpki::DNSNameRef::try_from_ascii_str(DEV_HOSTNAME).unwrap();
    let mut sess = rustls::ClientSession::new(&Arc::new(config), dns_name);
    let mut sock = TcpStream::new(fd).unwrap();
    let mut tls = rustls::Stream::new(&mut sess, &mut sock);

    let _result = tls.write(req.as_bytes());
    let mut plaintext = Vec::new();

    println!("write complete");

    match tls.read_to_end(&mut plaintext) {
        Ok(_) => (),
        Err(e) => {
            println!("get_sigrl_from_intel tls.read_to_end: {:?}", e);
            panic!("haha");
        }
    }
    println!("read_to_end complete");
    let resp_string = String::from_utf8(plaintext.clone()).unwrap();

    println!("{}", resp_string);

    parse_response_sigrl(&plaintext)
}

// TODO: support pse
pub fn get_report_from_intel(fd : c_int, quote : Vec<u8>) -> (String, String, String) {
    println!("get_report_from_intel fd = {:?}", fd);
    let config = make_ias_client_config();
    let encoded_quote = base64::encode(&quote[..]);
    let encoded_json = format!("{{\"isvEnclaveQuote\":\"{}\"}}\r\n", encoded_quote);

    let req = format!("POST {} HTTP/1.1\r\nHOST: {}\r\nContent-Length:{}\r\nContent-Type: application/json\r\n\r\n{}",
                      REPORT_SUFFIX,
                      DEV_HOSTNAME,
                      encoded_json.len(),
                      encoded_json);
    println!("{}", req);
    let dns_name = webpki::DNSNameRef::try_from_ascii_str(DEV_HOSTNAME).unwrap();
    let mut sess = rustls::ClientSession::new(&Arc::new(config), dns_name);
    let mut sock = TcpStream::new(fd).unwrap();
    let mut tls = rustls::Stream::new(&mut sess, &mut sock);

    let _result = tls.write(req.as_bytes());
    let mut plaintext = Vec::new();

    println!("write complete");

    tls.read_to_end(&mut plaintext).unwrap();
    println!("read_to_end complete");
    let resp_string = String::from_utf8(plaintext.clone()).unwrap();

    println!("resp_string = {}", resp_string);

    let (attn_report, sig, cert) = parse_response_attn_report(&plaintext);

    (attn_report, sig, cert)
}

fn as_u32_le(array: &[u8; 4]) -> u32 {
    ((array[0] as u32) <<  0) +
    ((array[1] as u32) <<  8) +
    ((array[2] as u32) << 16) +
    ((array[3] as u32) << 24)
}

#[allow(const_err)]
pub fn create_attestation_report(pub_k: &sgx_ec256_public_t, sign_type: sgx_quote_sign_type_t) -> Result<(String, String, String), sgx_status_t> {
    // Workflow:
    // (1) ocall to get the target_info structure (ti) and epid group id (eg)
    // (1.5) get sigrl
    // (2) call sgx_create_report with ti+data, produce an sgx_report_t
    // (3) ocall to sgx_get_quote to generate (*mut sgx-quote_t, uint32_t)

    // (1) get ti + eg
    let mut ti : sgx_target_info_t = sgx_target_info_t::default();
    let mut eg : sgx_epid_group_id_t = sgx_epid_group_id_t::default();
    let mut rt : sgx_status_t = sgx_status_t::SGX_ERROR_UNEXPECTED;

    let res = unsafe {
        ocall_sgx_init_quote(&mut rt as *mut sgx_status_t,
                             &mut ti as *mut sgx_target_info_t,
                             &mut eg as *mut sgx_epid_group_id_t)
    };

    println!("eg = {:?}", eg);

    if res != sgx_status_t::SGX_SUCCESS {
        return Err(res);
    }

    if rt != sgx_status_t::SGX_SUCCESS {
        return Err(rt);
    }

    let eg_num = as_u32_le(&eg);

    // (1.5) get sigrl
    let mut ias_sock : i32 = 0;

    let res = unsafe {
        ocall_get_ias_socket(&mut rt as *mut sgx_status_t,
                             &mut ias_sock as *mut i32)
    };

    if res != sgx_status_t::SGX_SUCCESS {
        return Err(res);
    }

    if rt != sgx_status_t::SGX_SUCCESS {
        return Err(rt);
    }

    //println!("Got ias_sock = {}", ias_sock);

    // Now sigrl_vec is the revocation list, a vec<u8>
    let sigrl_vec : Vec<u8> = get_sigrl_from_intel(ias_sock, eg_num);

    // (2) Generate the report
    // Fill ecc256 public key into report_data
    let mut report_data: sgx_report_data_t = sgx_report_data_t::default();
    let mut pub_k_gx = pub_k.gx.clone();
    pub_k_gx.reverse();
    let mut pub_k_gy = pub_k.gy.clone();
    pub_k_gy.reverse();
    report_data.d[..32].clone_from_slice(&pub_k_gx);
    report_data.d[32..].clone_from_slice(&pub_k_gy);

    let rep = match rsgx_create_report(&ti, &report_data) {
        Ok(r) =>{
            println!("Report creation => success {:?}", r.body.mr_signer.m);
            Some(r)
        },
        Err(e) =>{
            println!("Report creation => failed {:?}", e);
            None
        },
    };

    let mut quote_nonce = sgx_quote_nonce_t { rand : [0;16] };
    let mut os_rng = os::SgxRng::new().unwrap();
    os_rng.fill_bytes(&mut quote_nonce.rand);
    println!("rand finished");
    let mut qe_report = sgx_report_t::default();
    const RET_QUOTE_BUF_LEN : u32 = 2048;
    let mut return_quote_buf : [u8; RET_QUOTE_BUF_LEN as usize] = [0;RET_QUOTE_BUF_LEN as usize];
    let mut quote_len : u32 = 0;

    // (3) Generate the quote
    // Args:
    //       1. sigrl: ptr + len
    //       2. report: ptr 432bytes
    //       3. linkable: u32, unlinkable=0, linkable=1
    //       4. spid: sgx_spid_t ptr 16bytes
    //       5. sgx_quote_nonce_t ptr 16bytes
    //       6. p_sig_rl + sigrl size ( same to sigrl)
    //       7. [out]p_qe_report need further check
    //       8. [out]p_quote
    //       9. quote_size
    let (p_sigrl, sigrl_len) =
        if sigrl_vec.len() == 0 {
            (ptr::null(), 0)
        } else {
            (sigrl_vec.as_ptr(), sigrl_vec.len() as u32)
        };
    let p_report = (&rep.unwrap()) as * const sgx_report_t;
    let quote_type = sign_type;

    let spid : sgx_spid_t = load_spid("spid.txt");

    let p_spid = &spid as *const sgx_spid_t;
    let p_nonce = &quote_nonce as * const sgx_quote_nonce_t;
    let p_qe_report = &mut qe_report as *mut sgx_report_t;
    let p_quote = return_quote_buf.as_mut_ptr();
    let maxlen = RET_QUOTE_BUF_LEN;
    let p_quote_len = &mut quote_len as *mut u32;

    let result = unsafe {
    ocall_get_quote(&mut rt as *mut sgx_status_t,
                    p_sigrl,
                    sigrl_len,
                    p_report,
                    quote_type,
                    p_spid,
                    p_nonce,
                    p_qe_report,
                    p_quote,
                    maxlen,
                    p_quote_len)
    };

    if result != sgx_status_t::SGX_SUCCESS {
        return Err(result);
    }
    if rt != sgx_status_t::SGX_SUCCESS {
        println!("ocall_get_quote returned {}", rt);
        return Err(rt);
    }

    // Added 09-28-2018
    // Perform a check on qe_report to verify if the qe_report is valid
    match rsgx_verify_report(&qe_report) {
        Ok(()) => println!("rsgx_verify_report passed!"),
        Err(x) => {
            println!("rsgx_verify_report failed with {:?}", x);
            return Err(x);
        },
    }

    // Check if the qe_report is produced on the same platform
    if ti.mr_enclave.m != qe_report.body.mr_enclave.m ||
       ti.attributes.flags != qe_report.body.attributes.flags ||
       ti.attributes.xfrm  != qe_report.body.attributes.xfrm {
        println!("qe_report does not match current target_info!");
        return Err(sgx_status_t::SGX_ERROR_UNEXPECTED);
    }

    println!("qe_report check passed");

    // Debug
    // for i in 0..quote_len {
    //     print!("{:02X}", unsafe {*p_quote.offset(i as isize)});
    // }
    // println!("");

    // Check qe_report to defend against replay attack
    // The purpose of p_qe_report is for the ISV enclave to confirm the QUOTE
    // it received is not modified by the untrusted SW stack, and not a replay.
    // The implementation in QE is to generate a REPORT targeting the ISV
    // enclave (target info from p_report) , with the lower 32Bytes in
    // report.data = SHA256(p_nonce||p_quote). The ISV enclave can verify the
    // p_qe_report and report.data to confirm the QUOTE has not be modified and
    // is not a replay. It is optional.

    let mut rhs_vec : Vec<u8> = quote_nonce.rand.to_vec();
    rhs_vec.extend(&return_quote_buf[..quote_len as usize]);
    let rhs_hash = rsgx_sha256_slice(&rhs_vec[..]).unwrap();
    let lhs_hash = &qe_report.body.report_data.d[..32];

    println!("rhs hash = {:02X}", rhs_hash.iter().format(""));
    println!("report hs= {:02X}", lhs_hash.iter().format(""));

    if rhs_hash != lhs_hash {
        println!("Quote is tampered!");
        return Err(sgx_status_t::SGX_ERROR_UNEXPECTED);
    }

    let quote_vec : Vec<u8> = return_quote_buf[..quote_len as usize].to_vec();
    let res = unsafe {
        ocall_get_ias_socket(&mut rt as *mut sgx_status_t,
                             &mut ias_sock as *mut i32)
    };

    if res != sgx_status_t::SGX_SUCCESS {
        return Err(res);
    }

    if rt != sgx_status_t::SGX_SUCCESS {
        return Err(rt);
    }

    let (attn_report, sig, cert) = get_report_from_intel(ias_sock, quote_vec);
    Ok((attn_report, sig, cert))
}

fn load_spid(filename: &str) -> sgx_spid_t {
    let mut spidfile = fs::File::open(filename).expect("cannot open spid file");
    let mut contents = String::new();
    spidfile.read_to_string(&mut contents).expect("cannot read the spid file");

    hex::decode_spid(&contents)
}

fn load_certs(filename: &str) -> Vec<rustls::Certificate> {
    let certfile = fs::File::open(filename).expect("cannot open certificate file");
    let mut reader = BufReader::new(certfile);
    match rustls::internal::pemfile::certs(&mut reader) {
        Ok(r) => return r,
        Err(e) => {
            println!("Err in load_certs: {:?}", e);
            panic!("");
        }
    }
}

fn load_private_key(filename: &str) -> rustls::PrivateKey {
    let rsa_keys = {
        let keyfile = fs::File::open(filename)
            .expect("cannot open private key file");
        let mut reader = BufReader::new(keyfile);
        rustls::internal::pemfile::rsa_private_keys(&mut reader)
            .expect("file contains invalid rsa private key")
    };

    let pkcs8_keys = {
        let keyfile = fs::File::open(filename)
            .expect("cannot open private key file");
        let mut reader = BufReader::new(keyfile);
        rustls::internal::pemfile::pkcs8_private_keys(&mut reader)
            .expect("file contains invalid pkcs8 private key (encrypted keys not supported)")
    };

    if !pkcs8_keys.is_empty() {
        pkcs8_keys[0].clone()
    } else {
        assert!(!rsa_keys.is_empty());
        rsa_keys[0].clone()
    }
}

#[no_mangle]
pub extern "C" fn key_provision(socket_fd : c_int, sign_type: sgx_quote_sign_type_t) -> sgx_status_t {
    // Generate Keypair
    let ecc_handle = SgxEccHandle::new();
    let _result = ecc_handle.open();
    let (prv_k, pub_k) = ecc_handle.create_key_pair().unwrap();

    let (attn_report, sig, cert) = match create_attestation_report(&pub_k, sign_type) {
        Ok(r) => r,
        Err(e) => {
            println!("Error in create_attestation_report: {:?}", e);
            return e;
        }
    };

    let payload = attn_report + "|" + &sig + "|" + &cert;
    let (key_der, cert_der) = match cert::gen_ecc_cert(payload, &prv_k, &pub_k, &ecc_handle) {
        Ok(r) => r,
        Err(e) => {
            println!("Error in gen_ecc_cert: {:?}", e);
            return e;
        }
    };
    let _result = ecc_handle.close();

    let root_ca_bin = include_bytes!("../../../cert/ca.crt");
    let mut ca_reader = BufReader::new(&root_ca_bin[..]);
    let mut rc_store = rustls::RootCertStore::empty();
    // Build a root ca storage
    rc_store.add_pem_file(&mut ca_reader).unwrap();
    // Build a default authenticator which allow every authenticated client

    let authenticator = rustls::AllowAnyAuthenticatedClient::new(rc_store);
    let mut cfg = rustls::ServerConfig::new(authenticator);
    let mut certs = Vec::new();
    certs.push(rustls::Certificate(cert_der));
    let privkey = rustls::PrivateKey(key_der);

    cfg.set_single_cert_with_ocsp_and_sct(certs, privkey, vec![], vec![]).unwrap();

    let mut sess = rustls::ServerSession::new(&Arc::new(cfg));
    let mut conn = TcpStream::new(socket_fd).unwrap();

    let mut tls = rustls::Stream::new(&mut sess, &mut conn);
    let key_bin : Vec<u8>;

    match tls_receive_vec(&mut tls) {
        Ok(v) => {
            println!("Server received: {:?}", v);
            if v.len() != SGX_AESGCM_KEY_SIZE {
                println!("Keylen {} != {}", v.len(), SGX_AESGCM_KEY_SIZE);
                return sgx_status_t::SGX_ERROR_UNEXPECTED;
            }
            key_bin = v;
        },
        Err(ref err) if err.kind() == io::ErrorKind::ConnectionAborted => {
            println!("EOF (tls)");
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        },
        Err(e) => {
            println!("Error in read_to_end: {:?}", e);
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        },
    }

    tls_send_vec(&mut tls, b"RECV complete".to_vec()).unwrap();

    println!("Provision complete");
    println!("Incoming key_bin = {:02X}", key_bin.iter().format(""));

    match SgxFile::create(KEYFILE) {
        Ok(mut f) => {
            match f.write_all(&key_bin) {
                Ok(()) => {
                    println!("SgxFile write key file success!");
                    sgx_status_t::SGX_SUCCESS
                },
                Err(x) => {
                    println!("SgxFile write key file failed! {}", x);
                    sgx_status_t::SGX_ERROR_UNEXPECTED
                }
            }
        },
        Err(x) => {
            println!("SgxFile create file {} error {}", KEYFILE, x);
            sgx_status_t::SGX_ERROR_UNEXPECTED
        },
    }

}

#[no_mangle]
pub extern "C"
fn get_sealed_pcl_key_len() -> u32 {
    sgx_tseal::SgxSealedData::<[u8;SGX_AESGCM_KEY_SIZE]>::calc_raw_sealed_data_size(SGX_PCL_GUID_SIZE as u32, SGX_AESGCM_KEY_SIZE as u32)
}

#[no_mangle]
pub extern "C"
fn get_sealed_pcl_key(key_buf : *mut u8, key_len: u32) -> sgx_status_t {
    println!("Entering get_sealed_pcl_key");

    let mut key_array: [u8;SGX_AESGCM_KEY_SIZE] = [0;SGX_AESGCM_KEY_SIZE];
    match SgxFile::open(KEYFILE) {
        Ok(mut f) => {
            let mut keyvec : Vec<u8> = Vec::new();
            match f.read_to_end(&mut keyvec) {
                Ok(SGX_AESGCM_KEY_SIZE) => {
                    println!("SgxFs read success: {:02X}", keyvec.iter().format(""));
                    for i in 0..SGX_AESGCM_KEY_SIZE {
                        key_array[i] = keyvec[i];
                    }
                },
                Ok(len) => {
                    println!("SgxFs read len {} incorrect", len);
                    return sgx_status_t::SGX_ERROR_UNEXPECTED;
                },
                Err(x) => {
                    println!("Read keyfile failed {}", x);
                    return sgx_status_t::SGX_ERROR_UNEXPECTED;
                }
            }
        },
        Err(x) => {
            println!("get_sealed_pcl_key cannot open keyfile, please check if key is provisioned successfully! {}", x);
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    };

    let sealed_blob = match sgx_tseal::SgxSealedData::<[u8;SGX_AESGCM_KEY_SIZE]>::seal_data(&SGX_PCL_GUID, &key_array) {
        Ok(b) => b,
        Err(x) => {
            println!("sgx seal data failed {}!", x);
            return x;
        }
    };

    match unsafe { sealed_blob.to_raw_sealed_data_t(key_buf as *mut sgx_types::sgx_sealed_data_t, key_len) } {
        Some(_) => {
            sgx_status_t::SGX_SUCCESS
        },
        None => {
            println!("to_raw_sealed_data_t error!");
            return sgx_status_t::SGX_ERROR_INVALID_PARAMETER;
        }
    }
}
