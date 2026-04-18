//! 로컬 호스트 egress IP 감지.
//!
//! 서브계정 격리 환경에서 프로세스가 어떤 IP 로 나가는지 확인하는 유틸리티다.

use std::net::{IpAddr, UdpSocket};

use tracing::warn;

/// UDP connect trick 으로 egress IP 를 감지한다.
///
/// 실제 패킷을 보내지 않고 OS 라우팅 테이블만 참조한다.
pub fn detect_local_ip() -> Option<IpAddr> {
    match detect_local_ip_inner() {
        Ok(ip) => Some(ip),
        Err(e) => {
            warn!(
                target: "hft_account::ip_detect",
                error = %e,
                "failed to detect local IP"
            );
            None
        }
    }
}

fn detect_local_ip_inner() -> std::io::Result<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    Ok(socket.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_without_panic() {
        let _ = detect_local_ip();
    }
}
