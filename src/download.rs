//! Streams an extract to disk. Never publishes a partial file under its final name: the
//! runner treats "file exists" as "already downloaded", and a truncated .pbf would bake into
//! a region-shaped hole that nothing downstream can tell from genuinely empty land.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// `hint` is the size the plan recorded, used only to draw a percentage. It is NOT what the
/// download is checked against: Geofabrik rebuilds every extract daily, so a plan made
/// yesterday describes a file that no longer exists, and brazil arriving 344 KB larger than
/// planned is the archive being fresh, not the download being wrong. Completeness is judged
/// against the Content-Length of this very response.
/// Attempts per extract. Over 305 downloads and several hours a transient failure is close to
/// certain - one killed a run 31 regions in ("request or response body error") - and losing
/// hours of baking to a blip is not a reasonable failure mode.
const ATTEMPTS: u32 = 4;

pub fn fetch(url: &str, dir: &Path, hint: u64, quiet: bool) -> Result<PathBuf, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match fetch_once(url, dir, hint, quiet) {
            Ok(p) => return Ok(p),
            Err(e) => {
                last = e;
                if attempt < ATTEMPTS {
                    let wait = 5 * (1 << (attempt - 1));
                    eprintln!("    download failed ({last}); retrying in {wait}s");
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                }
            }
        }
    }
    Err(format!("after {ATTEMPTS} attempts: {last}"))
}

fn fetch_once(url: &str, dir: &Path, hint: u64, quiet: bool) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = url.rsplit('/').next().ok_or("bad url")?;
    let final_path = dir.join(name);
    let part = dir.join(format!("{name}.part"));

    // Only a completed download ever wears the final name, so any non-empty file here is one.
    if let Ok(md) = std::fs::metadata(&final_path) {
        if md.len() > 0 {
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
    let declared = resp.content_length();
    let total = declared.unwrap_or(hint);

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
        if !quiet && last_report.elapsed().as_secs() >= 5 {
            let pct = if total > 0 {
                format!(" ({:.0}%)", 100.0 * done as f64 / total as f64)
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
    if !quiet {
        eprintln!("\r    downloaded {:.0} MB          ", done as f64 / 1e6);
    }

    // A server that sent no Content-Length gives nothing to check against; a chunked transfer
    // that ends early is indistinguishable from one that ends, and the bake would reject the
    // truncated .pbf anyway.
    if let Some(declared) = declared {
        if done != declared {
            let _ = std::fs::remove_file(&part);
            return Err(format!("short download: got {done} of {declared} bytes"));
        }
    }
    std::fs::rename(&part, &final_path).map_err(|e| e.to_string())?;
    Ok(final_path)
}
