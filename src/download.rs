//! Streams an extract to disk. Never publishes a partial file under its final name: the
//! runner treats "file exists" as "already downloaded", and a truncated .pbf would bake into
//! a region-shaped hole that nothing downstream can tell from genuinely empty land.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one extract keeps being retried. Over 394 downloads and most of a day a failure is
/// close to certain - one killed a run 31 regions in ("request or response body error"), and
/// another 13 hours in, when download.openstreetmap.fr answered 502 to everything for 7.6
/// hours against the 40 seconds of retries it was then given. Waiting out an outage costs idle
/// hours; giving up throws away every hour already baked, and a run takes well under the other
/// 14 of Modal's 24.
const PATIENCE: Duration = Duration::from_secs(10 * 60 * 60);
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// `hint` is the size the plan recorded, used only to draw a percentage. It is NOT what the
/// download is checked against: Geofabrik rebuilds every extract daily, so a plan made
/// yesterday describes a file that no longer exists, and brazil arriving 344 KB larger than
/// planned is the archive being fresh, not the download being wrong. Completeness is judged
/// against the Content-Length of this very response.
pub fn fetch(url: &str, dir: &Path, hint: u64, quiet: bool) -> Result<PathBuf, String> {
    let started = Instant::now();
    let mut wait = Duration::from_secs(5);
    let mut attempts = 0;
    loop {
        attempts += 1;
        let last = match fetch_once(url, dir, hint, quiet) {
            Ok(p) => return Ok(p),
            Err(e) => e,
        };
        if started.elapsed() + wait > PATIENCE {
            return Err(format!(
                "after {attempts} attempts over {} min: {last}",
                started.elapsed().as_secs() / 60
            ));
        }
        eprintln!(
            "    download failed ({last}); retrying in {}s",
            wait.as_secs()
        );
        std::thread::sleep(wait);
        wait = (wait * 2).min(MAX_WAIT);
    }
}

/// Host and path, flattened. The basename alone collides: Geofabrik has a `georgia` in Europe
/// and one under `us`, and openstreetmap.fr's `kanto` shares Geofabrik's file name. A file here
/// counts as already downloaded, so a collision would bake the wrong region.
fn local_name(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, rest)| rest)
        .replace(['/', '\\', ':'], "_")
}

fn fetch_once(url: &str, dir: &Path, hint: u64, quiet: bool) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = local_name(url);
    let name = name.as_str();
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

#[cfg(test)]
mod tests {
    use super::local_name;

    #[test]
    fn same_basename_different_files() {
        let a = local_name("https://download.geofabrik.de/europe/georgia-latest.osm.pbf");
        let b = local_name("https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf");
        let c = local_name(
            "https://download.openstreetmap.fr/extracts/asia/japan/kanto-latest.osm.pbf",
        );
        let d = local_name("https://download.geofabrik.de/asia/japan/kanto-latest.osm.pbf");
        assert_ne!(a, b);
        assert_ne!(c, d);
        assert!(!a.contains('/'));
    }
}
