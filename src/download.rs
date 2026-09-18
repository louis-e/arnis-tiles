//! Streams an extract to disk. Never publishes a partial file under its final name: the
//! runner treats "file exists" as "already downloaded", and a truncated .pbf would bake into
//! a region-shaped hole that nothing downstream can tell from genuinely empty land.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn fetch(url: &str, dir: &Path, expected: u64) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = url.rsplit('/').next().ok_or("bad url")?;
    let final_path = dir.join(name);
    let part = dir.join(format!("{name}.part"));

    if let Ok(md) = std::fs::metadata(&final_path) {
        if expected == 0 || md.len() == expected {
            return Ok(final_path);
        }
        std::fs::remove_file(&final_path).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(&part);

    let client = reqwest::blocking::Client::builder()
        .user_agent(crate::plan::USER_AGENT)
        .timeout(None)
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{} for {url}", resp.status()));
    }

    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    let mut last_report = std::time::Instant::now();
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if last_report.elapsed().as_secs() >= 5 {
            let pct = if expected > 0 {
                format!(" ({:.0}%)", 100.0 * done as f64 / expected as f64)
            } else {
                String::new()
            };
            eprint!("\r    downloaded {:.0} MB{pct}    ", done as f64 / 1e6);
            let _ = std::io::stderr().flush();
            last_report = std::time::Instant::now();
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    eprintln!("\r    downloaded {:.0} MB          ", done as f64 / 1e6);

    if expected > 0 && done != expected {
        let _ = std::fs::remove_file(&part);
        return Err(format!("short download: got {done} of {expected} bytes"));
    }
    std::fs::rename(&part, &final_path).map_err(|e| e.to_string())?;
    Ok(final_path)
}
