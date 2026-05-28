use anyhow::{Context, Result};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Checks if the `xorriso` command-line utility is installed and available in the system PATH.
///
/// Returns an error if the command cannot be executed.
pub fn check_xorriso() -> Result<()> {
    let output = Command::new("xorriso")
        .arg("-version")
        .output()
        .context("Failed to execute xorriso. Is it installed? (Try: apt install xorriso)")?;

    if !output.status.success() {
        anyhow::bail!("xorriso command failed.");
    }
    Ok(())
}

/// Downloads a Debian ISO from the specified URL to a local destination file.
///
/// Displays a progress bar during the download.
///
/// # Arguments
/// * `url` - The URL of the ISO to download.
/// * `dest` - The path where the ISO should be saved.
pub async fn download_iso(url: &str, dest: &Path) -> Result<()> {
    println!("Downloading Debian ISO from {}", url);
    let client = Client::new();
    let res = client
        .get(url)
        .send()
        .await
        .context("Failed to send request for ISO")?;

    let total_size = res
        .content_length()
        .context("Failed to get content length from response")?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut file = File::create(dest).context("Failed to create destination file")?;
    let mut stream = res.bytes_stream();

    while let Some(item) = stream.next().await {
        let chunk = item.context("Error while downloading chunk")?;
        file.write_all(&chunk)
            .context("Error writing chunk to file")?;
        pb.inc(chunk.len() as u64);
    }

    pb.finish_with_message("Download complete");
    Ok(())
}

/// Injects a preseed file into the Debian installer ISO, modifying boot parameters
/// to enable an unattended installation.
///
/// This involves extracting the `initrd.gz` file, appending the preseed configuration
/// to it, modifying bootloader menus, and repacking the ISO.
///
/// # Arguments
/// * `original_iso` - Path to the unmodified Debian ISO.
/// * `preseed_file` - Path to the user's `preseed.cfg` file.
/// * `output_iso` - Path where the customized ISO will be written.
/// * `username` - Optional username to override in the preseed configuration.
/// * `password` - Optional password to override in the preseed configuration.
/// * `hostname` - Optional hostname to override in the preseed configuration.
pub fn inject_preseed(
    original_iso: &Path,
    preseed_file: &Path,
    output_iso: &Path,
    username: Option<&str>,
    password: Option<&str>,
    hostname: Option<&str>,
    serial_console: bool,
) -> Result<()> {
    println!("Injecting preseed.cfg into ISO...");

    let temp_dir = tempfile::tempdir()?;
    let initrd_gz_path = temp_dir.path().join("initrd.gz");

    // First try install.amd/initrd.gz, then install/initrd.gz
    let mut extract_status = Command::new("xorriso")
        .args([
            "-osirrox",
            "on",
            "-indev",
            original_iso.to_str().context("Invalid ISO path UTF-8")?,
            "-extract",
            "/install.amd/initrd.gz",
            initrd_gz_path
                .to_str()
                .context("Invalid initrd path UTF-8")?,
        ])
        .output()?;

    let mut initrd_iso_path = "/install.amd/initrd.gz";

    if !extract_status.status.success() || !initrd_gz_path.exists() {
        // Try alternate path for 32-bit or older ISOs
        extract_status = Command::new("xorriso")
            .args([
                "-osirrox",
                "on",
                "-indev",
                original_iso.to_str().context("Invalid ISO path UTF-8")?,
                "-extract",
                "/install/initrd.gz",
                initrd_gz_path
                    .to_str()
                    .context("Invalid initrd path UTF-8")?,
            ])
            .output()?;
        initrd_iso_path = "/install/initrd.gz";
    }

    if !extract_status.status.success() || !initrd_gz_path.exists() {
        anyhow::bail!(
            "Failed to extract initrd.gz from ISO. Is this a valid Debian installer ISO?"
        );
    }

    let preseed_content =
        fs::read_to_string(preseed_file).context("Failed to read preseed file")?;

    let modified_content = modify_preseed(&preseed_content, username, password, hostname);

    // Append to initrd.gz
    let mut perms = fs::metadata(&initrd_gz_path)?.permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&initrd_gz_path, perms)?;
    let initrd_file = OpenOptions::new().append(true).open(&initrd_gz_path)?;

    // Compress with gzip and append
    let mut encoder = flate2::write::GzEncoder::new(initrd_file, flate2::Compression::best());

    // Create cpio newc archive containing the preseed file
    let builder = cpio::NewcBuilder::new("preseed.cfg").mode(0o100644);
    let input = vec![(builder, std::io::Cursor::new(modified_content))];
    cpio::write_cpio(input.into_iter(), &mut encoder)?;

    encoder.finish()?;

    // Patch bootloader configs to bypass the menu and boot the automated install immediately
    let isolinux_cfg_path = temp_dir.path().join("isolinux.cfg");
    let txt_cfg_path = temp_dir.path().join("txt.cfg");
    let grub_cfg_path = temp_dir.path().join("grub.cfg");

    let output = Command::new("xorriso")
        .args([
            "-osirrox",
            "on",
            "-indev",
            original_iso.to_str().context("Invalid ISO path UTF-8")?,
            "-extract",
            "/isolinux/isolinux.cfg",
            isolinux_cfg_path
                .to_str()
                .context("Invalid isolinux_cfg path")?,
            "-extract",
            "/isolinux/txt.cfg",
            txt_cfg_path.to_str().context("Invalid txt_cfg path")?,
            "-extract",
            "/boot/grub/grub.cfg",
            grub_cfg_path.to_str().context("Invalid grub_cfg path")?,
        ])
        .output()?;
    if !output.status.success() {
        println!(
            "xorriso extract configs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    if isolinux_cfg_path.exists() {
        let mut iso_cfg = fs::read_to_string(&isolinux_cfg_path).unwrap_or_default();
        iso_cfg = iso_cfg.replace("timeout 0", "timeout 10");
        iso_cfg = iso_cfg.replace("default vesamenu.c32", "default install");
        let _ = fs::remove_file(&isolinux_cfg_path);
        fs::write(&isolinux_cfg_path, iso_cfg)?;
    }

    let extra_kernel_args = if serial_console {
        "console=ttyS0,115200n8 auto=true priority=critical --- quiet"
    } else {
        "auto=true priority=critical --- quiet"
    };

    if txt_cfg_path.exists() {
        let mut txt_cfg = fs::read_to_string(&txt_cfg_path).unwrap_or_default();
        txt_cfg = txt_cfg.replace(
            "--- quiet",
            extra_kernel_args,
        );
        let _ = fs::remove_file(&txt_cfg_path);
        fs::write(&txt_cfg_path, txt_cfg)?;
    }

    if grub_cfg_path.exists() {
        let mut grub_cfg = fs::read_to_string(&grub_cfg_path).unwrap_or_default();
        grub_cfg.insert_str(0, "set timeout=1\nset default=\"Install\"\n");
        grub_cfg = grub_cfg.replace(
            "--- quiet",
            extra_kernel_args,
        );
        let _ = fs::remove_file(&grub_cfg_path);
        fs::write(&grub_cfg_path, grub_cfg)?;
    }

    // Repack ISO with xorriso
    println!("Repacking ISO...");
    let mut repack_args = vec![
        "-indev".to_string(),
        original_iso.display().to_string(),
        "-outdev".to_string(),
        output_iso.display().to_string(),
        "-overwrite".to_string(),
        "on".to_string(),
        "-map".to_string(),
        initrd_gz_path.display().to_string(),
        initrd_iso_path.to_string(),
    ];
    if isolinux_cfg_path.exists() {
        repack_args.push("-map".to_string());
        repack_args.push(isolinux_cfg_path.display().to_string());
        repack_args.push("/isolinux/isolinux.cfg".to_string());
    }
    if txt_cfg_path.exists() {
        repack_args.push("-map".to_string());
        repack_args.push(txt_cfg_path.display().to_string());
        repack_args.push("/isolinux/txt.cfg".to_string());
    }
    if grub_cfg_path.exists() {
        repack_args.push("-map".to_string());
        repack_args.push(grub_cfg_path.display().to_string());
        repack_args.push("/boot/grub/grub.cfg".to_string());
    }
    repack_args.push("-boot_image".to_string());
    repack_args.push("any".to_string());
    repack_args.push("replay".to_string());

    let repack_status = Command::new("xorriso").args(&repack_args).status()?;

    if !repack_status.success() {
        anyhow::bail!("Failed to repack ISO with xorriso");
    }

    Ok(())
}

/// Writes the customized ISO image to a target block device (e.g., a USB drive).
///
/// # Arguments
/// * `iso_path` - Path to the customized ISO file.
/// * `device_path` - Path to the target block device to overwrite.
pub fn flash_to_usb(iso_path: &Path, device_path: &Path) -> Result<()> {
    println!("Flashing ISO to device {:?}...", device_path);
    println!(
        "WARNING: This will overwrite all data on {:?}.",
        device_path
    );

    let mut iso_file = File::open(iso_path).context("Failed to open custom ISO")?;
    let mut device_file = OpenOptions::new()
        .write(true)
        .open(device_path)
        .context(format!(
            "Failed to open device {:?} (Try running with sudo?)",
            device_path
        ))?;

    let total_size = iso_file.metadata()?.len();
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.red/yellow}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut buffer = [0; 4 * 1024 * 1024]; // 4MB buffer
    loop {
        let bytes_read = io::Read::read(&mut iso_file, &mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        device_file.write_all(&buffer[..bytes_read])?;

        // Ensure data is synced to disk for block devices
        device_file.sync_all()?;

        pb.inc(bytes_read as u64);
    }

    pb.finish_with_message("Flash complete");
    Ok(())
}

/// Scrapes the Debian mirrors to find the URL of the latest stable AMD64 DVD-1 ISO.
///
/// It fetches the `SHA512SUMS` file from the mirror, parses it to find the ISO filename,
/// and constructs the full download URL.
pub async fn get_latest_debian_iso_url() -> Result<String> {
    println!("Fetching latest Debian ISO information...");
    let base_url = "https://cdimage.debian.org/debian-cd/current/amd64/iso-dvd";
    let client = Client::new();
    let sums_url = format!("{}/SHA512SUMS", base_url);

    let sums_text = client
        .get(&sums_url)
        .send()
        .await
        .context("Failed to fetch SHA512SUMS from Debian mirror")?
        .text()
        .await
        .context("Failed to read SHA512SUMS text")?;

    let mut filename = None;
    for line in sums_text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }

        let name = parts[1];
        if name.starts_with("debian-")
            && name.ends_with("-amd64-DVD-1.iso")
            && !name.contains("-mac-")
            && !name.contains("-edu-")
        {
            filename = Some(name.to_string());
            break;
        }
    }

    if let Some(name) = filename {
        let url = format!("{}/{}", base_url, name);
        println!("Resolved latest stable ISO: {}", url);
        Ok(url)
    } else {
        anyhow::bail!(
            "Could not determine the latest stable ISO filename from {}",
            sums_url
        );
    }
}

/// Applies overrides to the provided preseed configuration content.
///
/// This function scans the content line by line and replaces specific d-i keys
/// (`passwd/username`, `passwd/user-password`, `passwd/user-password-again`,
/// and `netcfg/get_hostname`) with the provided overrides. If a requested override
/// key is not found in the original content, it is appended to the end.
///
/// # Arguments
/// * `content` - The original preseed configuration string.
/// * `username` - Optional username override.
/// * `password` - Optional password override.
/// * `hostname` - Optional hostname override.
///
/// # Returns
/// A new string containing the modified preseed configuration.
pub fn modify_preseed(
    content: &str,
    username: Option<&str>,
    password: Option<&str>,
    hostname: Option<&str>,
) -> String {
    let mut modified_content = String::new();
    let mut username_replaced = false;
    let mut password_replaced = false;
    let mut password_again_replaced = false;
    let mut hostname_replaced = false;

    for line in content.lines() {
        let trimmed = line.trim_start();

        if let Some(user) = username {
            if trimmed.starts_with("d-i passwd/username string") {
                modified_content.push_str(&format!("d-i passwd/username string {}\n", user));
                username_replaced = true;
                continue;
            }
        }

        if let Some(pass) = password {
            if trimmed.starts_with("d-i passwd/user-password password") {
                modified_content.push_str(&format!("d-i passwd/user-password password {}\n", pass));
                password_replaced = true;
                continue;
            }
            if trimmed.starts_with("d-i passwd/user-password-again password") {
                modified_content.push_str(&format!(
                    "d-i passwd/user-password-again password {}\n",
                    pass
                ));
                password_again_replaced = true;
                continue;
            }
        }

        if let Some(host) = hostname {
            if trimmed.starts_with("d-i netcfg/get_hostname string") {
                modified_content.push_str(&format!("d-i netcfg/get_hostname string {}\n", host));
                hostname_replaced = true;
                continue;
            }
        }

        modified_content.push_str(line);
        modified_content.push('\n');
    }

    if let Some(user) = username {
        if !username_replaced {
            modified_content.push_str(&format!("d-i passwd/username string {}\n", user));
        }
    }
    if let Some(pass) = password {
        if !password_replaced {
            modified_content.push_str(&format!("d-i passwd/user-password password {}\n", pass));
        }
        if !password_again_replaced {
            modified_content.push_str(&format!(
                "d-i passwd/user-password-again password {}\n",
                pass
            ));
        }
    }
    if let Some(host) = hostname {
        if !hostname_replaced {
            modified_content.push_str(&format!("d-i netcfg/get_hostname string {}\n", host));
        }
    }

    modified_content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modify_preseed_replaces_existing() {
        let original =
            "d-i passwd/username string olduser\nd-i netcfg/get_hostname string oldhost\n";
        let result = modify_preseed(original, Some("newuser"), None, Some("newhost"));
        assert!(result.contains("d-i passwd/username string newuser"));
        assert!(result.contains("d-i netcfg/get_hostname string newhost"));
        assert!(!result.contains("olduser"));
        assert!(!result.contains("oldhost"));
    }

    #[test]
    fn test_modify_preseed_appends_missing() {
        let original = "d-i some/other/key boolean true\n";
        let result = modify_preseed(original, Some("newuser"), Some("pass123"), None);
        assert!(result.contains("d-i some/other/key boolean true"));
        assert!(result.contains("d-i passwd/username string newuser"));
        assert!(result.contains("d-i passwd/user-password password pass123"));
        assert!(result.contains("d-i passwd/user-password-again password pass123"));
    }

    #[test]
    fn test_modify_preseed_no_overrides() {
        let original = "d-i passwd/username string root\n";
        let result = modify_preseed(original, None, None, None);
        assert_eq!(original, result);
    }
}
