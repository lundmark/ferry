//! Requires Docker. Run with: cargo test --test ftp_integration -- --ignored
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use ferry::ftp::Ftp;

fn start_ftp() -> (String, u16, Container<GenericImage>) {
    let img = GenericImage::new("delfer/alpine-ftp-server", "latest")
        .with_exposed_port(21.tcp())
        .with_wait_for(WaitFor::message_on_stderr("vsftpd"))
        .with_env_var("USERS", "test|testpw|/home/test");
    let container = img.start().unwrap();
    let port = container.get_host_port_ipv4(21.tcp()).unwrap();
    ("127.0.0.1".into(), port, container)
}

#[test]
#[ignore]
fn connect_list_upload_download() {
    let (host, port, _c) = start_ftp();
    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    ftp.upload_bytes("/hello.txt", b"hello\n").unwrap();
    let listed = ftp.list("/").unwrap();
    assert!(listed.iter().any(|e| e.name == "hello.txt"));
    let bytes = ftp.download("/hello.txt").unwrap();
    assert_eq!(bytes, b"hello\n");
}
