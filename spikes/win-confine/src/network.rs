use crate::native::*;
use serde_json::json;
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
};
use windows_sys::Win32::Networking::WinSock::*;

fn socket_error(t: &Target, stage: &str, error: i32, startup: i32) -> Probe {
    Probe {
        kind: t.kind.clone(),
        target: t.name.clone(),
        access: t.access.clone(),
        success: false,
        result: json!({"domain":"Winsock","code":error,"stage":stage,"startup_code":startup}),
    }
}
pub fn probes(tcp_port: u16, udp_port: u16, mode: &str) -> Vec<Probe> {
    unsafe {
        // std::net panics when WSAStartup is denied. A denial must become evidence,
        // not destroy the completed file/IPC measurements in this process.
        let mut data: WSADATA = zeroed();
        let startup = WSAStartup(0x202, &mut data);
        let mut results = Vec::new();
        for (kind, port, socket_kind, protocol) in [
            ("tcp", tcp_port, SOCK_STREAM, IPPROTO_TCP),
            ("udp", udp_port, SOCK_DGRAM, IPPROTO_UDP),
        ] {
            let t = Target::new(
                kind,
                format!("127.0.0.1:{port}"),
                if kind == "tcp" {
                    "socket + connect"
                } else {
                    "socket + sendto"
                },
            );
            let sock = socket(AF_INET as i32, socket_kind, protocol);
            if sock == INVALID_SOCKET {
                results.push(socket_error(&t, "socket", WSAGetLastError(), startup));
                continue;
            }
            let mut address: SOCKADDR_IN = zeroed();
            address.sin_family = AF_INET;
            address.sin_port = port.to_be();
            address.sin_addr.S_un.S_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            let (ok, error, stage) = if kind == "tcp" {
                let mut nonblocking = 1;
                let nonblock_ok = ioctlsocket(sock, FIONBIO, &mut nonblocking) == 0;
                if !nonblock_ok {
                    (false, WSAGetLastError(), "ioctlsocket")
                } else {
                    let status = connect(
                        sock,
                        (&address as *const SOCKADDR_IN).cast(),
                        size_of::<SOCKADDR_IN>() as i32,
                    );
                    let error = if status == 0 { 0 } else { WSAGetLastError() };
                    if error == WSAEWOULDBLOCK {
                        let mut write: FD_SET = zeroed();
                        write.fd_count = 1;
                        write.fd_array[0] = sock;
                        let mut except = write;
                        let timeout = TIMEVAL {
                            tv_sec: 2,
                            tv_usec: 0,
                        };
                        let status = select(0, null_mut(), &mut write, &mut except, &timeout);
                        if status == 0 {
                            (false, WSAETIMEDOUT, "connect/select")
                        } else if status == SOCKET_ERROR {
                            (false, WSAGetLastError(), "select")
                        } else {
                            let mut socket_error_code = 0i32;
                            let mut bytes = size_of::<i32>() as i32;
                            if getsockopt(
                                sock,
                                SOL_SOCKET,
                                SO_ERROR,
                                (&mut socket_error_code as *mut i32).cast(),
                                &mut bytes,
                            ) == SOCKET_ERROR
                            {
                                (false, WSAGetLastError(), "getsockopt(SO_ERROR)")
                            } else {
                                (socket_error_code == 0, socket_error_code, "connect")
                            }
                        }
                    } else {
                        (status == 0, error, "connect")
                    }
                }
            } else {
                let status = sendto(
                    sock,
                    mode.as_ptr(),
                    mode.len() as i32,
                    0,
                    (&address as *const SOCKADDR_IN).cast(),
                    size_of::<SOCKADDR_IN>() as i32,
                );
                (
                    status != SOCKET_ERROR,
                    if status == SOCKET_ERROR {
                        WSAGetLastError()
                    } else {
                        0
                    },
                    "sendto",
                )
            };
            closesocket(sock);
            results.push(if ok {Probe {kind:t.kind,target:t.name,access:t.access,success:true,result:json!({"domain":"Winsock","code":0,"stage":stage,"startup_code":startup})}}else{socket_error(&t,stage,error,startup)});
        }
        if startup == 0 {
            WSACleanup();
        }
        results
    }
}
