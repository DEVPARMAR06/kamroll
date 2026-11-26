//! The real iPhone over USB, read-only.
//!
//! usbmuxd (Apple's USB service) -> the phone's lockdown service (paired, TLS) -> the AFC file service.
//! It never writes to the phone: the only AFC operations used are list, stat, open-for-reading, seek, read, close.
//!
//! NOTE: this was written from the protocol and checked against the packet layout only. It has not yet been run
//! against a real iPhone, so the Rust engine is not the app's default engine until it has.

use crate::device::{Device, DeviceInfo, RemoteFile};
use plist::{Dictionary, Value};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use std::io::{Read, Write};
#[cfg(not(unix))]
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}
type Stream = Box<dyn Conn>;

const AFC_MAGIC: &[u8; 8] = b"CFA6LPAA";
const OP_STATUS: u64 = 1;
const OP_DATA: u64 = 2;
const OP_READ_DIR: u64 = 3;
const OP_GET_FILE_INFO: u64 = 0xA;
const OP_FILE_OPEN: u64 = 0xD;
const OP_FILE_OPEN_RES: u64 = 0xE;
const OP_FILE_READ: u64 = 0xF;
const OP_FILE_SEEK: u64 = 0x11;
const OP_FILE_CLOSE: u64 = 0x14;
const MAX_READ: usize = 4 << 20;

const IPHONE_NAMES: [(&str, &str); 16] = [
    ("iPhone14,2", "iPhone 13 Pro"), ("iPhone14,3", "iPhone 13 Pro Max"), ("iPhone14,4", "iPhone 13 mini"), ("iPhone14,5", "iPhone 13"),
    ("iPhone14,7", "iPhone 14"), ("iPhone14,8", "iPhone 14 Plus"), ("iPhone15,2", "iPhone 14 Pro"), ("iPhone15,3", "iPhone 14 Pro Max"),
    ("iPhone15,4", "iPhone 15"), ("iPhone15,5", "iPhone 15 Plus"), ("iPhone16,1", "iPhone 15 Pro"), ("iPhone16,2", "iPhone 15 Pro Max"),
    ("iPhone17,1", "iPhone 16 Pro"), ("iPhone17,2", "iPhone 16 Pro Max"), ("iPhone17,3", "iPhone 16"), ("iPhone17,4", "iPhone 16 Plus"),
];

// ---- usbmuxd ----

fn mux_socket() -> Result<Stream, String> {
    #[cfg(unix)]
    let s: Stream = {
        let u = std::os::unix::net::UnixStream::connect("/var/run/usbmuxd").map_err(|e| format!("usbmuxd is not running: {e}"))?;
        let _ = u.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = u.set_write_timeout(Some(Duration::from_secs(30)));
        Box::new(u)
    };
    #[cfg(not(unix))]
    let s: Stream = {
        let t = TcpStream::connect_timeout(&"127.0.0.1:27015".parse().unwrap(), Duration::from_secs(3)).map_err(|e| format!("usbmuxd is not running: {e}"))?;
        let _ = t.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = t.set_write_timeout(Some(Duration::from_secs(30)));
        Box::new(t)
    };
    Ok(s)
}

fn to_xml(d: Dictionary) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &Value::Dictionary(d)).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn mux_request(s: &mut Stream, message: &str, mut extra: Dictionary) -> Result<Dictionary, String> {
    extra.insert("MessageType".into(), message.into());
    extra.insert("ClientVersionString".into(), "photovault".into());
    extra.insert("ProgName".into(), "photovault".into());
    let body = to_xml(extra)?;
    let mut pkt = Vec::with_capacity(16 + body.len());
    for v in [16 + body.len() as u32, 1, 8, 1] {
        pkt.extend_from_slice(&v.to_le_bytes());
    }
    pkt.extend_from_slice(&body);
    s.write_all(&pkt).map_err(|e| format!("usbmuxd socket: {e}"))?;
    let mut h = [0u8; 16];
    s.read_exact(&mut h).map_err(|e| format!("usbmuxd socket: {e}"))?;
    let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len.saturating_sub(16)];
    s.read_exact(&mut payload).map_err(|e| format!("usbmuxd socket: {e}"))?;
    Value::from_reader(std::io::Cursor::new(payload)).ok().and_then(|v| v.into_dictionary()).ok_or_else(|| "usbmuxd sent something unreadable".to_string())
}

fn mux_connect(device_id: u64, port: u16) -> Result<Stream, String> {
    let mut s = mux_socket()?;
    let mut d = Dictionary::new();
    d.insert("DeviceID".into(), Value::Integer(device_id.into()));
    d.insert("PortNumber".into(), Value::Integer((port.swap_bytes() as u64).into()));
    let r = mux_request(&mut s, "Connect", d)?;
    match r.get("Number").and_then(|n| n.as_unsigned_integer()) {
        Some(0) => Ok(s),
        n => Err(format!("The iPhone refused the connection (usbmuxd result {})", n.unwrap_or(99))),
    }
}

// ---- TLS (lockdown pairing) ----

#[derive(Debug)]
struct AcceptAll;

impl ServerCertVerifier for AcceptAll {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion()) // the phone's own certificate; trust comes from the pairing, as with Apple's tools
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        use SignatureScheme::*;
        vec![RSA_PKCS1_SHA1, RSA_PKCS1_SHA256, RSA_PKCS1_SHA384, RSA_PKCS1_SHA512, RSA_PSS_SHA256, RSA_PSS_SHA384, RSA_PSS_SHA512, ECDSA_NISTP256_SHA256, ECDSA_NISTP384_SHA384]
    }
}

fn tls_wrap(tcp: Stream, cert_pem: &[u8], key_pem: &[u8]) -> Result<Stream, String> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..]).flatten().collect();
    let key = rustls_pemfile::private_key(&mut &key_pem[..]).map_err(|e| e.to_string())?.ok_or("The pairing record has no private key")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAll))
        .with_client_auth_cert(certs, key)
        .map_err(|e| e.to_string())?;
    let conn = ClientConnection::new(Arc::new(cfg), ServerName::try_from("iphone.local").map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(Box::new(StreamOwned::new(conn, tcp)))
}

// ---- lockdown ----

fn lockdown_call(s: &mut Stream, mut d: Dictionary) -> Result<Dictionary, String> {
    d.insert("Label".into(), "photovault".into());
    let body = to_xml(d)?;
    s.write_all(&(body.len() as u32).to_be_bytes()).and_then(|_| s.write_all(&body)).map_err(|e| format!("lockdown socket: {e}"))?;
    let mut h = [0u8; 4];
    s.read_exact(&mut h).map_err(|e| format!("lockdown socket: {e}"))?;
    let mut payload = vec![0u8; u32::from_be_bytes(h) as usize];
    s.read_exact(&mut payload).map_err(|e| format!("lockdown socket: {e}"))?;
    let r = Value::from_reader(std::io::Cursor::new(payload)).ok().and_then(|v| v.into_dictionary()).ok_or("lockdown sent something unreadable")?;
    if let Some(e) = r.get("Error").and_then(|e| e.as_string()) {
        return Err(match e {
            "PasswordProtected" => "The iPhone is locked. Unlock it and tap Trust if asked.".to_string(),
            "InvalidHostID" | "UserDeniedPairing" | "PairingDialogResponsePending" => "The iPhone does not trust this computer yet. Unlock it and tap Trust.".to_string(),
            other => format!("lockdown error: {other}"),
        });
    }
    Ok(r)
}

fn req(pairs: &[(&str, Value)]) -> Dictionary {
    let mut d = Dictionary::new();
    for (k, v) in pairs {
        d.insert((*k).into(), v.clone());
    }
    d
}

fn pair_record(udid: &str) -> Result<Dictionary, String> {
    if let Ok(mut s) = mux_socket() {
        if let Ok(r) = mux_request(&mut s, "ReadPairRecord", req(&[("PairRecordID", udid.into())])) {
            if let Some(Value::Data(bytes)) = r.get("PairRecordData") {
                if let Some(d) = Value::from_reader(std::io::Cursor::new(bytes.clone())).ok().and_then(|v| v.into_dictionary()) {
                    return Ok(d);
                }
            }
        }
    }
    let dirs: Vec<std::path::PathBuf> = vec![
        std::path::PathBuf::from(std::env::var("ProgramData").unwrap_or_default()).join("Apple").join("Lockdown"),
        std::path::PathBuf::from("/var/db/lockdown"),
    ];
    for d in dirs {
        if let Ok(v) = Value::from_file(d.join(format!("{udid}.plist"))) {
            if let Some(d) = v.into_dictionary() {
                return Ok(d);
            }
        }
    }
    Err("No pairing record for this iPhone. Unlock it and tap Trust.".into())
}

struct Link {
    afc: Stream,
    packet: u64,
    handle: Option<(String, u64, u64)>, // path, handle, read position
    info: DeviceInfo,
}

fn connect() -> Result<Link, String> {
    let mut mux = mux_socket()?;
    let list = mux_request(&mut mux, "ListDevices", Dictionary::new())?;
    let dev = list
        .get("DeviceList")
        .and_then(|v| v.as_array())
        .and_then(|a| a.iter().filter_map(|d| d.as_dictionary()).find(|d| d.get("Properties").and_then(|p| p.as_dictionary()).and_then(|p| p.get("ConnectionType")).and_then(|c| c.as_string()) == Some("USB")))
        .ok_or("NoDeviceConnectedError: no iPhone found")?;
    let props = dev.get("Properties").and_then(|p| p.as_dictionary()).ok_or("NoDeviceConnectedError")?;
    let device_id = props.get("DeviceID").and_then(|v| v.as_unsigned_integer()).ok_or("NoDeviceConnectedError")?;
    let udid = props.get("SerialNumber").and_then(|v| v.as_string()).ok_or("NoDeviceConnectedError")?.to_string();
    let pair = pair_record(&udid)?;
    let host_id = pair.get("HostID").and_then(|v| v.as_string()).unwrap_or_default().to_string();
    let buid = pair.get("SystemBUID").and_then(|v| v.as_string()).unwrap_or_default().to_string();
    let (cert, key) = match (pair.get("HostCertificate"), pair.get("HostPrivateKey")) {
        (Some(Value::Data(c)), Some(Value::Data(k))) => (c.clone(), k.clone()),
        _ => return Err("The pairing record is incomplete. Unlock the iPhone and tap Trust again.".into()),
    };

    let mut ld = mux_connect(device_id, 62078)?;
    let sess = lockdown_call(&mut ld, req(&[("Request", "StartSession".into()), ("HostID", host_id.into()), ("SystemBUID", buid.into()), ("ProtocolVersion", "2".into())]))?;
    if sess.get("EnableSessionSSL").and_then(|v| v.as_boolean()).unwrap_or(false) {
        ld = tls_wrap(ld, &cert, &key)?;
    }
    let all = lockdown_call(&mut ld, req(&[("Request", "GetValue".into())]))?;
    let values = all.get("Value").and_then(|v| v.as_dictionary()).cloned().unwrap_or_default();
    let text = |k: &str| values.get(k).and_then(|v| v.as_string()).unwrap_or("").to_string();
    let pt = text("ProductType");
    let model = IPHONE_NAMES.iter().find(|(k, _)| *k == pt).map(|(_, v)| v.to_string()).unwrap_or(pt);
    let name = if text("DeviceName").is_empty() { "iPhone".to_string() } else { text("DeviceName") };
    let svc = lockdown_call(&mut ld, req(&[("Request", "StartService".into()), ("Service", "com.apple.afc".into())]))?;
    let port = svc.get("Port").and_then(|v| v.as_unsigned_integer()).ok_or("The iPhone did not start its file service")? as u16;
    let mut afc = mux_connect(device_id, port)?;
    if svc.get("EnableServiceSSL").and_then(|v| v.as_boolean()).unwrap_or(false) {
        afc = tls_wrap(afc, &cert, &key)?;
    }
    Ok(Link { afc, packet: 0, handle: None, info: DeviceInfo { name, model, ios: text("ProductVersion") } })
}

// ---- AFC ----

impl Link {
    fn rpc(&mut self, op: u64, head: &[u8], data: &[u8]) -> Result<(u64, Vec<u8>), String> {
        self.packet += 1;
        let this_len = 40 + head.len() as u64;
        let mut pkt = Vec::with_capacity(this_len as usize + data.len());
        pkt.extend_from_slice(AFC_MAGIC);
        for v in [this_len + data.len() as u64, this_len, self.packet, op] {
            pkt.extend_from_slice(&v.to_le_bytes());
        }
        pkt.extend_from_slice(head);
        pkt.extend_from_slice(data);
        self.afc.write_all(&pkt).map_err(|e| format!("socket error: {e}"))?;
        let mut h = [0u8; 40];
        self.afc.read_exact(&mut h).map_err(|e| format!("socket error: {e}"))?;
        if &h[..8] != AFC_MAGIC {
            return Err("AfcError: bad reply from the iPhone".into());
        }
        let total = u64::from_le_bytes(h[8..16].try_into().unwrap());
        let resp_op = u64::from_le_bytes(h[32..40].try_into().unwrap());
        let mut payload = vec![0u8; total.saturating_sub(40) as usize];
        self.afc.read_exact(&mut payload).map_err(|e| format!("socket error: {e}"))?;
        if resp_op == OP_STATUS {
            let code = payload.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
            if code != 0 && code != 14 {
                return Err(format!("AfcError: the iPhone answered with status {code}"));
            }
        }
        Ok((resp_op, payload))
    }

    fn path(p: &str) -> Vec<u8> {
        let mut v = p.as_bytes().to_vec();
        v.push(0);
        v
    }

    fn listdir(&mut self, p: &str) -> Result<Vec<String>, String> {
        let (_, data) = self.rpc(OP_READ_DIR, &Self::path(p), &[])?;
        Ok(data.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).filter(|s| s != "." && s != "..").collect())
    }

    fn stat(&mut self, p: &str) -> Result<std::collections::HashMap<String, String>, String> {
        let (_, data) = self.rpc(OP_GET_FILE_INFO, &Self::path(p), &[])?;
        let parts: Vec<String> = data.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect();
        Ok(parts.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect())
    }

    fn close_handle(&mut self) {
        if let Some((_, h, _)) = self.handle.take() {
            let _ = self.rpc(OP_FILE_CLOSE, &h.to_le_bytes(), &[]);
        }
    }

    fn pread(&mut self, path: &str, offset: u64, n: usize) -> Result<Vec<u8>, String> {
        if self.handle.as_ref().map(|(p, _, _)| p != path).unwrap_or(true) {
            self.close_handle();
            let mut head = 1u64.to_le_bytes().to_vec(); // read-only
            head.extend(Self::path(path));
            let (op, payload) = self.rpc(OP_FILE_OPEN, &head, &[])?;
            if op != OP_FILE_OPEN_RES || payload.len() < 8 {
                return Err(format!("AfcError: could not open {path}"));
            }
            self.handle = Some((path.to_string(), u64::from_le_bytes(payload[..8].try_into().unwrap()), 0));
        }
        let (_, h, pos) = self.handle.clone().unwrap();
        if pos != offset {
            let mut head = h.to_le_bytes().to_vec();
            head.extend(0u64.to_le_bytes()); // from the start
            head.extend(offset.to_le_bytes());
            self.rpc(OP_FILE_SEEK, &head, &[])?;
        }
        let mut head = h.to_le_bytes().to_vec();
        head.extend((n.min(MAX_READ) as u64).to_le_bytes());
        let (op, data) = self.rpc(OP_FILE_READ, &head, &[])?;
        let data = if op == OP_DATA { data } else { vec![] };
        if let Some(hd) = self.handle.as_mut() {
            hd.2 = offset + data.len() as u64;
        }
        Ok(data)
    }
}

pub struct AfcDevice {
    link: Option<Link>,
}

impl AfcDevice {
    pub fn new() -> AfcDevice {
        AfcDevice { link: None }
    }
    fn link(&mut self) -> Result<&mut Link, String> {
        if self.link.is_none() {
            self.link = Some(connect()?);
        }
        Ok(self.link.as_mut().unwrap())
    }
}

fn secs(ns: Option<&String>) -> Option<f64> {
    ns.and_then(|s| s.parse::<f64>().ok()).map(|n| n / 1e9)
}

impl Device for AfcDevice {
    fn list_files(&mut self, roots: &[String], ignore: &[String], each: &mut dyn FnMut(RemoteFile) -> bool) -> Result<(), String> {
        let l = self.link()?;
        for root in roots {
            let mut folders = l.listdir(root)?;
            folders.sort();
            for folder in folders {
                if crate::device::ignored(&folder, ignore) {
                    continue;
                }
                let fpath = format!("{}/{folder}", root.trim_end_matches('/'));
                let st = l.stat(&fpath)?;
                let entries: Vec<(String, String)> = if st.get("st_ifmt").map(|s| s.contains("DIR")).unwrap_or(false) {
                    l.listdir(&fpath)?.into_iter().map(|n| (n.clone(), format!("{fpath}/{n}"))).collect()
                } else {
                    vec![(folder.clone(), fpath.clone())] // a loose file directly under the root
                };
                for (n, p) in entries {
                    if crate::device::ignored(&n, ignore) {
                        continue;
                    }
                    let s = l.stat(&p)?;
                    if !s.get("st_ifmt").map(|v| v.contains("REG")).unwrap_or(false) {
                        continue;
                    }
                    let modified = secs(s.get("st_mtime")).unwrap_or(0.0);
                    let created = secs(s.get("st_birthtime")).filter(|c| *c > 0.0).unwrap_or(modified);
                    let size = s.get("st_size").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
                    if !each(RemoteFile { path: p, size, created, modified }) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn read_range(&mut self, path: &str, offset: u64, n: usize) -> Result<Vec<u8>, String> {
        let r = self.link()?.pread(path, offset, n);
        if r.is_err() {
            self.link = None; // a dropped connection is rebuilt by reconnect()
        }
        r
    }

    fn reconnect(&mut self) -> Result<(), String> {
        self.link = None;
        self.link = Some(connect()?);
        Ok(())
    }

    fn info(&mut self) -> Result<DeviceInfo, String> {
        let l = connect()?;
        Ok(l.info)
    }
}
