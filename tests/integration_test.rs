use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Tests the full installation cycle of the Debian installer.
///
/// It uses QEMU to boot the customized ISO and verifies the installation
/// by attempting an SSH connection to the newly installed VM.
#[test]
// This test takes 5-15 minutes to run and requires qemu, qemu-img, and sshpass
fn test_full_installation_cycle() {
    let target_tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
    fs::create_dir_all(&target_tmp).expect("Failed to create target/tmp");
    let temp_dir = tempfile::tempdir_in(&target_tmp).expect("Failed to create temp dir");
    let usb_img = temp_dir.path().join("usb.img");
    let hdd_img = temp_dir.path().join("hdd.qcow2");

    // Use the absolute path to the default preseed file in the root of the package
    let mut preseed_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    preseed_path.push("default_preseed.cfg");

    // 1. Download OVMF firmware for UEFI testing if not already cached
    let ovmf_tarball = target_tmp.join("ovmf.tar.xz");
    let ovmf_code = target_tmp.join("code.fd");
    let ovmf_vars = target_tmp.join("vars.fd");
    let test_ovmf_vars = temp_dir.path().join("vars.fd");

    if !ovmf_code.exists() {
        println!("Downloading prebuilt OVMF firmware...");
        let status = Command::new("curl")
            .args(&[
                "-sL",
                "https://github.com/rust-osdev/ovmf-prebuilt/releases/download/edk2-stable202605-r1/edk2-stable202605-r1-bin.tar.xz",
                "-o",
                ovmf_tarball.to_str().unwrap(),
            ])
            .status()
            .expect("Failed to run curl");
        assert!(status.success(), "Failed to download OVMF");

        println!("Extracting OVMF firmware...");
        let status = Command::new("tar")
            .args(&[
                "-xf",
                ovmf_tarball.to_str().unwrap(),
                "-C",
                target_tmp.to_str().unwrap(),
                "--strip-components=2",
                "edk2-stable202605-r1-bin/x64/code.fd",
                "edk2-stable202605-r1-bin/x64/vars.fd",
            ])
            .status()
            .expect("Failed to run tar");
        assert!(status.success(), "Failed to extract OVMF");
    }

    // Copy vars.fd to the temp_dir so QEMU can write to it without corrupting the cached copy
    fs::copy(&ovmf_vars, &test_ovmf_vars).expect("Failed to copy vars.fd");

    // 2. Create a dummy file for the USB block device so it can be opened with write(true)
    fs::File::create(&usb_img).expect("Failed to create usb.img");

    // 2. Run the debian_installer_builder binary to download the ISO, patch it, and flash it to usb_img
    println!("Running debian_installer_builder to create USB image...");
    println!("BIN PATH: {}", env!("CARGO_BIN_EXE_debian_installer_builder"));
    println!("PRESEED PATH: {}", preseed_path.display());
    let status = Command::new(env!("CARGO_BIN_EXE_debian_installer_builder"))
        .env("TMPDIR", target_tmp.to_str().unwrap())
        .args(&[
            "--device",
            usb_img.to_str().unwrap(),
            "--preseed",
            preseed_path.to_str().unwrap(),
            "--username",
            "testuser",
            "--password",
            "testpass",
            "--hostname",
            "testvm",
            "--serial-console",
        ])
        .status()
        .expect("Failed to execute debian_installer_builder");

    assert!(status.success(), "debian_installer_builder failed");

    // 3. Create a 10G qcow2 virtual hard drive
    println!("Creating virtual hard drive...");
    let status = Command::new("qemu-img")
        .args(&["create", "-f", "qcow2", hdd_img.to_str().unwrap(), "10G"])
        .status()
        .expect("Failed to run qemu-img");
    assert!(status.success(), "qemu-img failed");

    // Prepare QEMU args for installation
    let drive_hdd = format!("file={},format=qcow2,if=virtio", hdd_img.to_str().unwrap());
    // Expose the USB image as a regular block device to mimic a physical USB stick,
    // rather than a CD-ROM. This tests our partman/early_command logic.
    let drive_usb = format!("file={},format=raw,if=virtio", usb_img.to_str().unwrap());
    
    let drive_ovmf_code = format!("if=pflash,format=raw,readonly=on,file={}", ovmf_code.display());
    let drive_ovmf_vars = format!("if=pflash,format=raw,file={}", test_ovmf_vars.display());

    let mut qemu_install_args = vec![
        "-m",
        "2048",
        "-drive",
        &drive_usb, // Pass USB first so it becomes the primary boot device
        "-drive",
        &drive_hdd, // Pass HDD second
        "-boot",
        "c", // Boot from the first hard drive (USB)
        "-net",
        "nic",
        "-net",
        "user",
        "-display",
        "none",
        "-serial",
        "file:/tmp/qemu-install-serial.log",
        "-drive",
        &drive_ovmf_code,
        "-drive",
        &drive_ovmf_vars,
        "-no-reboot", // QEMU will exit automatically when the installer finishes and issues a reboot
    ];

    if std::path::Path::new("/dev/kvm").exists() {
        println!("KVM is available. Enabling hardware acceleration for faster installation.");
        qemu_install_args.push("-enable-kvm");
        qemu_install_args.push("-cpu");
        qemu_install_args.push("host");
    }

    // 4. Run QEMU to install Debian (unattended)
    println!("Booting QEMU for unattended installation. This will take a while...");
    let status = Command::new("qemu-system-x86_64")
        .args(&qemu_install_args)
        .status()
        .expect("Failed to run qemu for installation");
    assert!(status.success(), "Installation QEMU run failed");

    // Grab a random ephemeral port for SSH
    let ssh_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("Failed to bind random port for SSH")
        .local_addr()
        .unwrap()
        .port();
    let ssh_port_str = ssh_port.to_string();
    let netdev_arg = format!("user,id=net0,hostfwd=tcp::{}-:22", ssh_port);

    // Prepare QEMU args for the final boot
    let filter_dump_arg = format!("filter-dump,id=dump0,netdev=net0,file={}", temp_dir.path().join("mdns.pcap").display());
    let mut qemu_boot_args = vec![
        "-m",
        "1024",
        "-drive",
        &drive_hdd, // Only attach the HDD now
        "-netdev",
        &netdev_arg,
        "-device",
        "virtio-net-pci,netdev=net0",
        "-object",
        &filter_dump_arg,
        "-display",
        "none",
        "-drive",
        &drive_ovmf_code,
        "-drive",
        &drive_ovmf_vars,
    ];

    if std::path::Path::new("/dev/kvm").exists() {
        qemu_boot_args.push("-enable-kvm");
        qemu_boot_args.push("-cpu");
        qemu_boot_args.push("host");
    }

    // 5. Boot the installed system with port forwarding for SSH
    println!("Booting installed system...");
    let mut qemu_vm = Command::new("qemu-system-x86_64")
        .args(&qemu_boot_args)
        .spawn()
        .expect("Failed to start qemu for testing");

    // 6. Attempt SSH connection
    println!("Waiting for SSH to become available...");
    let start = Instant::now();
    let timeout = Duration::from_secs(300); // 5 minutes max wait for boot
    let mut success = false;

    while start.elapsed() < timeout {
        let ssh_status = Command::new("sshpass")
            .args(&[
                "-p",
                "testpass",
                "ssh",
                "-p",
                &ssh_port_str,
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "testuser@127.0.0.1",
                "echo",
                "SSH Connection Successful!",
            ])
            .status();

        if let Ok(s) = ssh_status {
            if s.success() {
                success = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    assert!(
        success,
        "Failed to connect via SSH to the newly installed VM"
    );

    // 7. Trigger an mDNS query internally to guarantee a packet is broadcast on the interface
    println!("Triggering mDNS broadcast...");
    let _ = Command::new("sshpass")
        .args(&[
            "-p",
            "testpass",
            "ssh",
            "-p",
            &ssh_port_str,
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "testuser@127.0.0.1",
            "ping",
            "-c",
            "1",
            "testvm.local",
        ])
        .status();

    // 7.5. Verify internet connectivity and package installation via APT mirrors
    println!("Verifying APT mirrors and git installation over SSH...");
    let apt_status = Command::new("sshpass")
        .args(&[
            "-p",
            "testpass",
            "ssh",
            "-p",
            &ssh_port_str,
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "testuser@127.0.0.1",
            "until ping -c1 deb.debian.org; do sleep 1; done && echo testpass | sudo -S apt-get update && echo testpass | sudo -S apt-get install -y git && git --version",
        ])
        .status()
        .expect("Failed to execute SSH command for APT");

    assert!(apt_status.success(), "Failed to install git via APT mirrors");

    // Kill the VM
    qemu_vm.kill().expect("Failed to kill QEMU VM");

    // 8. Verify mDNS resolution from the host by inspecting the raw network capture
    println!("Verifying mDNS advertisements from the host via PCAP...");
    let pcap_content = fs::read(temp_dir.path().join("mdns.pcap")).expect("Failed to read PCAP file");

    // Look for the mDNS string format of "testvm.local" inside the packet payloads
    let mdns_payload = b"\x06testvm\x05local";
    let mdns_success = pcap_content.windows(mdns_payload.len()).any(|window| window == mdns_payload);

    assert!(mdns_success, "Failed to find mDNS advertisement for testvm.local in the host network capture");
}
