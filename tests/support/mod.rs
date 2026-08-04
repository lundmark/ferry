use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use ferry::ftp::Ftp;
use testcontainers::{
    Container, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
};

pub const REMOTE_ROOT: &str = "/home/test";

pub struct FtpFixture {
    pub host: String,
    pub control_port: u16,
    pub container: Container<GenericImage>,
}

pub fn start_ftp() -> FtpFixture {
    let passive_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let passive_port = passive_listener.local_addr().unwrap().port();
    let control_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let control_port = control_listener.local_addr().unwrap().port();
    drop(passive_listener);
    drop(control_listener);

    let passive = passive_port.to_string();
    let image = GenericImage::new("delfer/alpine-ftp-server", "latest")
        .with_exposed_port(21.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("USERS", "test|testpw|/home/test")
        .with_env_var("ADDRESS", "127.0.0.1")
        .with_env_var("MIN_PORT", passive.clone())
        .with_env_var("MAX_PORT", passive)
        .with_mapped_port(control_port, 21.tcp())
        .with_mapped_port(passive_port, passive_port.tcp());
    let container = image.start().unwrap();
    let control_port = container.get_host_port_ipv4(21.tcp()).unwrap();
    let host = "127.0.0.1".to_string();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Ftp::connect(&host, control_port, "test", "testpw", true) {
            Ok(_) => break,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("FTP fixture did not become ready: {error:#}"),
        }
    }

    FtpFixture {
        host,
        control_port,
        container,
    }
}

pub fn remote_path(rel: &str) -> String {
    format!("{REMOTE_ROOT}/{}", rel.trim_start_matches('/'))
}

pub fn write_config(local_root: &std::path::Path, fixture: &FtpFixture) -> std::path::PathBuf {
    let path = local_root.join(".ferry.toml");
    std::fs::write(
        &path,
        format!(
            "[connection]\nhost = {:?}\nport = {}\nuser = \"test\"\npassword = \"testpw\"\npassive = true\n\n[paths]\nlocal_root = {:?}\nremote_root = {:?}\n",
            fixture.host,
            fixture.control_port,
            local_root.display().to_string(),
            REMOTE_ROOT,
        ),
    )
    .unwrap();
    path
}
