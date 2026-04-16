//! Manages Xvfb + Chrome + x11vnc + noVNC for browser-based Flipster login.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::error::FlipsterError;

const X11VNC_LIB: &str = "/tmp/x11vnc-extract/usr/lib/x86_64-linux-gnu";
const X11VNC_BIN: &str = "/tmp/x11vnc-extract/usr/bin/x11vnc";

pub struct BrowserManager {
    pub display: u32,
    pub vnc_port: u16,
    pub novnc_port: u16,
    pub chrome_debug_port: u16,
    pub chrome_user_data: String,
    pub start_url: String,
    procs: Vec<Child>,
}

impl Default for BrowserManager {
    fn default() -> Self {
        Self {
            display: 99,
            vnc_port: 5900,
            novnc_port: 6080,
            chrome_debug_port: 9222,
            chrome_user_data: "/tmp/chrome-flipster".into(),
            start_url: "https://flipster.io/trade/perpetual/BTCUSDT.PERP".into(),
            procs: Vec::new(),
        }
    }
}

impl BrowserManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start all services. Returns the noVNC URL.
    pub fn start(&mut self) -> Result<String, FlipsterError> {
        self.ensure_deps()?;
        self.start_xvfb()?;
        self.start_chrome()?;
        self.start_x11vnc()?;
        self.start_novnc()?;
        Ok(self.novnc_url())
    }

    /// Stop all child processes.
    pub fn stop(&mut self) {
        for proc in self.procs.iter_mut().rev() {
            let _ = proc.kill();
            let _ = proc.wait();
        }
        self.procs.clear();
    }

    pub fn novnc_url(&self) -> String {
        format!("http://0.0.0.0:{}/vnc.html", self.novnc_port)
    }

    /// Chrome DevTools Protocol WebSocket URL.
    pub async fn cdp_ws_url(&self) -> Result<String, FlipsterError> {
        let url = format!(
            "http://localhost:{}/json/version",
            self.chrome_debug_port
        );
        let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
        resp["webSocketDebuggerUrl"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| FlipsterError::Api {
                status: 0,
                message: "CDP webSocketDebuggerUrl not found".into(),
            })
    }

    fn ensure_deps(&self) -> Result<(), FlipsterError> {
        if !Path::new(X11VNC_BIN).exists() {
            return Err(FlipsterError::Api {
                status: 0,
                message: concat!(
                    "x11vnc not found. Run:\n",
                    "  cd /tmp && apt download x11vnc libvncserver1 libvncclient1\n",
                    "  dpkg-deb -x x11vnc_*.deb x11vnc-extract\n",
                    "  dpkg-deb -x libvncserver1_*.deb x11vnc-extract\n",
                    "  dpkg-deb -x libvncclient1_*.deb x11vnc-extract"
                )
                .into(),
            });
        }
        let novnc = dirs::home_dir()
            .unwrap_or_default()
            .join(".local/share/noVNC/utils/novnc_proxy");
        if !novnc.exists() {
            return Err(FlipsterError::Api {
                status: 0,
                message: format!(
                    "noVNC not found at {:?}. Run:\n  git clone --depth 1 \
                     https://github.com/novnc/noVNC.git ~/.local/share/noVNC",
                    novnc
                ),
            });
        }
        Ok(())
    }

    fn start_xvfb(&mut self) -> Result<(), FlipsterError> {
        let child = Command::new("Xvfb")
            .args([
                &format!(":{}", self.display),
                "-screen",
                "0",
                "1920x1080x24",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FlipsterError::Api {
                status: 0,
                message: format!("Failed to start Xvfb: {e}"),
            })?;
        self.procs.push(child);
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    }

    fn start_chrome(&mut self) -> Result<(), FlipsterError> {
        let child = Command::new("google-chrome")
            .env("DISPLAY", format!(":{}", self.display))
            .args([
                "--no-sandbox",
                "--disable-gpu",
                &format!("--remote-debugging-port={}", self.chrome_debug_port),
                "--remote-debugging-address=0.0.0.0",
                &format!("--user-data-dir={}", self.chrome_user_data),
                "--window-size=1920,1080",
                &self.start_url,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FlipsterError::Api {
                status: 0,
                message: format!("Failed to start Chrome: {e}"),
            })?;
        self.procs.push(child);
        std::thread::sleep(Duration::from_secs(2));
        Ok(())
    }

    fn start_x11vnc(&mut self) -> Result<(), FlipsterError> {
        let child = Command::new(X11VNC_BIN)
            .env("LD_LIBRARY_PATH", X11VNC_LIB)
            .args([
                "-display",
                &format!(":{}", self.display),
                "-nopw",
                "-forever",
                "-shared",
                "-rfbport",
                &self.vnc_port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FlipsterError::Api {
                status: 0,
                message: format!("Failed to start x11vnc: {e}"),
            })?;
        self.procs.push(child);
        std::thread::sleep(Duration::from_secs(1));
        Ok(())
    }

    fn start_novnc(&mut self) -> Result<(), FlipsterError> {
        let novnc = dirs::home_dir()
            .unwrap_or_default()
            .join(".local/share/noVNC/utils/novnc_proxy");
        let child = Command::new(novnc)
            .args([
                "--vnc",
                &format!("localhost:{}", self.vnc_port),
                "--listen",
                &self.novnc_port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FlipsterError::Api {
                status: 0,
                message: format!("Failed to start noVNC: {e}"),
            })?;
        self.procs.push(child);
        std::thread::sleep(Duration::from_secs(1));
        Ok(())
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        self.stop();
    }
}
