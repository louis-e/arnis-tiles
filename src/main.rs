//! arnis-tiles - bakes OpenStreetMap extracts into the Arnis tile archive.
//!
//! plan -> run -> finalize. `run` streams one extract at a time and deletes it again, so the
//! whole planet needs tens of gigabytes of disk rather than hundreds.

mod bake;
mod cells;
mod download;
mod finalize;
mod format;
mod plan;
mod store;
mod tags;
mod tilemath;

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// Where index/size lookups are cached.
    #[arg(long, default_value = "cache", global = true)]
    cache: PathBuf,
    /// Scratch: the chunk store and the extract currently being baked.
    #[arg(long, default_value = "work", global = true)]
    work: PathBuf,
    /// Finished archives.
    #[arg(long, default_value = "out", global = true)]
    out: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Work out which extracts cover the world, cheapest first. Writes work/plan.json.
    Plan,
    /// Download, bake and delete each planned extract. Resumable: finished regions are skipped.
    Run {
        /// Bake only these region ids (default: everything in the plan).
        #[arg(long, num_args = 1..)]
        only: Vec<String>,
        /// Keep each .pbf instead of deleting it after baking.
        #[arg(long)]
        keep_pbf: bool,
    },
    /// Publish any continent whose chunk store is still on disk. `run` already does this as
    /// each continent completes; this is for finishing an interrupted run.
    Finalize,
    /// Print what is in the chunk store.
    Status,
    /// Clear the bake state so the next `run` re-bakes, keeping the plan and the cached index.
    /// Without this a re-bake silently does nothing: finished continents are marked and skipped.
    Reset {
        /// Only this continent, so one archive can be refreshed without re-baking the planet.
        #[arg(long)]
        continent: Option<String>,
    },
    /// Decode one tile out of the chunk store (debugging).
    Inspect { x: u32, y: u32 },
    /// Backfill coverage cells into the index by scanning the published archives in out/.
    /// Only needed for archives baked before cells existed; `run` writes them itself.
    Cells,
}

/// Free bytes on the filesystem holding `path`, or None if it cannot be read.
///
/// The run writes tens of gigabytes over hours; filling the disk halfway through corrupts
/// nothing (the store is transactional) but wastes the whole night, so it stops early instead.
fn free_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let avail: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail * 1024)
}

/// Stop with this much still free, so `finalize` (which reclaims the stores) can actually run.
const MIN_FREE_BYTES: u64 = 15_000_000_000;

fn plan_path(work: &Path) -> PathBuf {
    work.join("plan.json")
}

/// A continent already published. Its chunk store is gone, so `run` must not try to add to it.
fn finalized_marker(work: &Path, continent: &str) -> PathBuf {
    work.join(format!("{continent}.finalized"))
}

/// Writes work/manifest.json and the published archives.json, returning the index path.
fn write_index(
    work: &Path,
    out: &Path,
    zoom: u8,
    manifest: &[finalize::ArchiveEntry],
) -> Result<PathBuf, String> {
    std::fs::write(
        work.join("manifest.json"),
        serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let index = out.join("archives.json");
    std::fs::write(
        &index,
        serde_json::to_vec_pretty(&serde_json::json!({
            "zoom": zoom,
            "format": "AOT1+zstd",
            "cell_zoom": cells::CELL_ZOOM,
            "attribution": "© OpenStreetMap contributors, ODbL 1.0",
            "archives": manifest,
        }))
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(index)
}

/// Publishes one continent and reclaims its chunk store.
///
/// Done as soon as a continent's last region is baked rather than at the very end, because all
/// the stores together do not fit: the planet's chunks come to ~103 GB, and turning each
/// continent into its (smaller) archive as it completes keeps peak disk near one store plus the
/// archives written so far.
fn publish_continent(work: &Path, out: &Path, zoom: u8, continent: &str) -> Result<(), String> {
    let store_path = store::store_path(work, continent);
    if !store_path.exists() {
        return Ok(());
    }
    {
        let st = store::ChunkStore::open(&store_path)?;
        if let Some(entry) = finalize::one(&st, out, zoom, continent)? {
            let manifest_path = work.join("manifest.json");
            let mut manifest: Vec<finalize::ArchiveEntry> = std::fs::read(&manifest_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let superseded = manifest
                .iter()
                .find(|e| e.name == continent && e.file != entry.file)
                .map(|e| e.file.clone());
            manifest.retain(|e| e.name != continent);
            manifest.push(entry);
            manifest.sort_by(|a, b| a.name.cmp(&b.name));
            let index = write_index(work, out, zoom, &manifest)?;
            println!("published {} and updated {}", continent, index.display());
            // Kept, not deleted: clients cache the index for 24h, so the previous archive has
            // to stay reachable until they have all seen the new one.
            if let Some(old) = superseded {
                println!("  superseded {old} - remove from out/ and R2 after 24h");
            }
        }
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", store_path.display()));
    }
    std::fs::write(finalized_marker(work, continent), b"").map_err(|e| e.to_string())?;
    Ok(())
}

fn load_plan(work: &Path) -> Result<plan::Plan, String> {
    let raw = std::fs::read(plan_path(work))
        .map_err(|e| format!("no plan yet (run `arnis-tiles plan` first): {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| e.to_string())
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.work).map_err(|e| e.to_string())?;

    match cli.cmd {
        Cmd::Plan => {
            let p = plan::build(&cli.cache, tilemath::ZOOM)?;
            std::fs::write(
                plan_path(&cli.work),
                serde_json::to_vec_pretty(&p).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            println!(
                "{} extracts, {:.1} GB to download",
                p.regions.len(),
                p.total_bytes as f64 / 1e9
            );
            println!(
                "sample coverage: {} points covered, {} uncovered (open sea)",
                p.covered_points, p.uncovered_points
            );
            println!("largest: ");
            for r in p.regions.iter().take(5) {
                println!("  {:28} {:.2} GB", r.id, r.bytes as f64 / 1e9);
            }
            println!("plan written to {}", plan_path(&cli.work).display());
        }

        Cmd::Run { only, keep_pbf } => {
            let p = load_plan(&cli.work)?;
            let pbf_dir = cli.work.join("pbf");
            let mut stores: HashMap<String, store::ChunkStore> = HashMap::new();
            // Grouped by continent, biggest continent first, so a continent completes and its
            // store is reclaimed before the next one starts - and the largest store exists
            // while the fewest archives are on disk.
            let mut continent_bytes: HashMap<&str, u64> = HashMap::new();
            for r in &p.regions {
                *continent_bytes.entry(r.continent.as_str()).or_default() += r.bytes;
            }
            let mut todo: Vec<&plan::Region> = p
                .regions
                .iter()
                .filter(|r| only.is_empty() || only.contains(&r.id))
                .filter(|r| !finalized_marker(&cli.work, &r.continent).exists())
                .collect();
            todo.sort_by_key(|r| {
                (
                    std::cmp::Reverse(
                        continent_bytes
                            .get(r.continent.as_str())
                            .copied()
                            .unwrap_or(0),
                    ),
                    r.continent.clone(),
                    std::cmp::Reverse(r.bytes),
                )
            });
            let total = todo.len();
            // The bake is CPU-bound and the download is not, so the next extract is fetched
            // while the current one bakes. Serially this run is download + bake; overlapped it
            // is roughly the slower of the two.
            let mut prefetch: Option<(String, std::thread::JoinHandle<Result<PathBuf, String>>)> =
                None;
            for (i, r) in todo.iter().enumerate() {
                if !stores.contains_key(&r.continent) {
                    let path = store::store_path(&cli.work, &r.continent);
                    stores.insert(r.continent.clone(), store::ChunkStore::open(&path)?);
                }
                if let Some(free) = free_bytes(&cli.work) {
                    if free < MIN_FREE_BYTES {
                        return Err(format!(
                            "only {:.1} GB free on the work disk; stopping before {}. Run \
                             `arnis-tiles finalize` to publish what is baked and reclaim the \
                             chunk stores, then resume this run.",
                            free as f64 / 1e9,
                            r.id
                        ));
                    }
                }
                let st = stores.get_mut(&r.continent).expect("store just inserted");
                let already = st.is_done(&r.id)?;
                if already {
                    println!("[{}/{total}] {} already baked, skipping", i + 1, r.id);
                }
                if !already {
                    println!(
                        "[{}/{total}] {} ({:.0} MB)",
                        i + 1,
                        r.id,
                        r.bytes as f64 / 1e6
                    );
                    // A previous crash may have left half of this region in the store.
                    st.clear_region(&r.id)?;
                    let path = match prefetch.take() {
                        Some((id, handle)) if id == r.id => {
                            handle.join().map_err(|_| "download thread panicked")??
                        }
                        other => {
                            // Either nothing was queued yet, or the queued one is not this region
                            // (a skip landed in between); that download still finished into the
                            // pbf dir, so it is simply not waited on here.
                            drop(other);
                            download::fetch(&r.url, &pbf_dir, r.bytes, false)?
                        }
                    };
                    if let Some(next) = todo.get(i + 1) {
                        if !st.is_done(&next.id)? {
                            let (url, dir, bytes, id) = (
                                next.url.clone(),
                                pbf_dir.clone(),
                                next.bytes,
                                next.id.clone(),
                            );
                            prefetch = Some((
                                id,
                                std::thread::spawn(move || {
                                    download::fetch(&url, &dir, bytes, true)
                                }),
                            ));
                        }
                    }
                    let t0 = std::time::Instant::now();
                    let s = bake::bake(&path, st, &r.id, p.zoom)?;
                    st.mark_done(&r.id, s.bytes, s.tiles)?;
                    if !keep_pbf {
                        let _ = std::fs::remove_file(&path);
                    }
                    println!(
                        "    {} ways, {} pois, {} relations -> {} tiles, {:.0} MB in {:.0}s",
                        s.ways,
                        s.nodes,
                        s.relations,
                        s.tiles,
                        s.bytes as f64 / 1e6,
                        t0.elapsed().as_secs_f64()
                    );
                }

                // Last region of this continent? Publish it now and give the disk back.
                // Checked for skipped regions too: a continent whose regions were all baked by
                // an earlier run would otherwise never be published at all, and its store would
                // sit on disk with no archive to show for it.
                let more_here = todo
                    .get(i + 1..)
                    .is_some_and(|rest| rest.iter().any(|n| n.continent == r.continent));
                if !more_here {
                    stores.remove(&r.continent);
                    publish_continent(&cli.work, &cli.out, p.zoom, &r.continent)?;
                }
            }
            println!("run complete");
        }

        Cmd::Finalize => {
            let p = load_plan(&cli.work)?;
            let mut continents: Vec<String> =
                p.regions.iter().map(|r| r.continent.clone()).collect();
            continents.sort();
            continents.dedup();

            for continent in continents {
                publish_continent(&cli.work, &cli.out, p.zoom, &continent)?;
            }
        }

        Cmd::Reset { continent } => {
            let mut removed = 0;
            for entry in std::fs::read_dir(&cli.work).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let stale = match &continent {
                    // manifest.json is left alone for a single continent: it still describes the
                    // other archives, and publishing this one rewrites only its own entry.
                    Some(c) => name == format!("chunks-{c}.db") || name == format!("{c}.finalized"),
                    None => {
                        name.starts_with("chunks-")
                            || name.ends_with(".finalized")
                            || name == "manifest.json"
                    }
                };
                if stale {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                    removed += 1;
                }
            }
            let _ = std::fs::remove_dir_all(cli.work.join("pbf"));
            match &continent {
                Some(c) => println!("cleared {removed} state files for {c}; `run` will re-bake it"),
                None => {
                    println!("cleared {removed} state files; plan.json and cache/ kept");
                    println!("out/ is untouched - move or delete it before re-baking");
                }
            }
        }

        Cmd::Status => {
            let p = load_plan(&cli.work)?;
            let mut continents: Vec<String> =
                p.regions.iter().map(|r| r.continent.clone()).collect();
            continents.sort();
            continents.dedup();
            let (mut all_regions, mut all_bytes, mut all_disk) = (0i64, 0i64, 0u64);
            for continent in &continents {
                let path = store::store_path(&cli.work, continent);
                if !path.exists() {
                    continue;
                }
                let st = store::ChunkStore::open(&path)?;
                let (regions, bytes): (i64, i64) = st
                    .connection()
                    .query_row(
                        "SELECT COUNT(*), COALESCE(SUM(bytes),0) FROM done",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .map_err(|e| e.to_string())?;
                let disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                println!(
                    "  {continent:20} {regions:4} regions, {:.1} GB of tiles, store {:.1} GB",
                    bytes as f64 / 1e9,
                    disk as f64 / 1e9
                );
                all_regions += regions;
                all_bytes += bytes;
                all_disk += disk;
            }
            println!(
                "{all_regions}/{} regions baked, {:.1} GB of tiles, {:.1} GB on disk",
                p.regions.len(),
                all_bytes as f64 / 1e9,
                all_disk as f64 / 1e9
            );
        }

        Cmd::Cells => {
            let manifest_path = cli.work.join("manifest.json");
            let mut manifest: Vec<finalize::ArchiveEntry> = serde_json::from_slice(
                &std::fs::read(&manifest_path).map_err(|e| format!("no manifest.json: {e}"))?,
            )
            .map_err(|e| e.to_string())?;
            for entry in &mut manifest {
                let path = cli.out.join(&entry.file);
                let found = cells::scan_archive(&path, tilemath::ZOOM)?;
                println!("{:24} {} cells", entry.name, found.len());
                entry.cells = found;
                if entry.built.is_empty() {
                    if let Ok(secs) = std::fs::metadata(&path).and_then(|m| {
                        m.modified()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .map_err(std::io::Error::other)
                    }) {
                        entry.built = finalize::date_from_secs(secs);
                    }
                }
            }
            let index = write_index(&cli.work, &cli.out, tilemath::ZOOM, &manifest)?;
            println!("updated {}", index.display());
        }

        Cmd::Inspect { x, y } => {
            let p = load_plan(&cli.work)?;
            let continent = p
                .regions
                .first()
                .map(|r| r.continent.clone())
                .unwrap_or_else(|| "europe".into());
            let st = store::ChunkStore::open(&store::store_path(&cli.work, &continent))?;
            let mut stmt = st
                .connection()
                .prepare("SELECT region, data FROM chunk WHERE x=?1 AND y=?2")
                .map_err(|e| e.to_string())?;
            let rows: Vec<(String, Vec<u8>)> = stmt
                .query_map(rusqlite::params![x, y], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| e.to_string())?
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            let (w, s, e, n) = tilemath::tile_bounds(x, y, tilemath::ZOOM);
            println!(
                "tile z{}/{x}/{y}  bbox {s:.5},{w:.5},{n:.5},{e:.5}",
                tilemath::ZOOM
            );
            for (region, blob) in rows {
                let t = format::decode(&store::ChunkStore::unpack(&blob)?)?;
                println!(
                    "  {region:24} {} ways, {} pois, {} relations, {} bytes",
                    t.ways.len(),
                    t.nodes.len(),
                    t.relations.len(),
                    blob.len()
                );
            }
        }
    }
    Ok(())
}
