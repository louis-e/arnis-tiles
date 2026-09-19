//! arnis-tiles - bakes OpenStreetMap extracts into the Arnis tile archive.
//!
//! plan -> run -> finalize. `run` streams one extract at a time and deletes it again, so the
//! whole planet needs tens of gigabytes of disk rather than hundreds.

mod bake;
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
    /// Merge each continent's chunk store into out/<continent>.pmtiles.
    Finalize {
        /// Keep the chunk stores. They are deleted by default: together they are the largest
        /// thing on disk, and re-running `run` for that continent rebuilds one.
        #[arg(long)]
        keep_chunks: bool,
    },
    /// Print what is in the chunk store.
    Status,
    /// Decode one tile out of the chunk store (debugging).
    Inspect { x: u32, y: u32 },
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
            let todo: Vec<&plan::Region> = p
                .regions
                .iter()
                .filter(|r| only.is_empty() || only.contains(&r.id))
                .collect();
            let total = todo.len();
            // The bake is CPU-bound and the download is not, so the next extract is fetched
            // while the current one bakes. Serially this run is download + bake; overlapped it
            // is roughly the slower of the two.
            let mut prefetch: Option<(String, std::thread::JoinHandle<Result<PathBuf, String>>)> = None;
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
                if st.is_done(&r.id)? {
                    println!("[{}/{total}] {} already baked, skipping", i + 1, r.id);
                    continue;
                }
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
                        let (url, dir, bytes, id) =
                            (next.url.clone(), pbf_dir.clone(), next.bytes, next.id.clone());
                        prefetch = Some((
                            id,
                            std::thread::spawn(move || download::fetch(&url, &dir, bytes, true)),
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
            println!("run complete");
        }

        Cmd::Finalize { keep_chunks } => {
            let p = load_plan(&cli.work)?;
            let mut continents: Vec<String> =
                p.regions.iter().map(|r| r.continent.clone()).collect();
            continents.sort();
            continents.dedup();

            let mut manifest: Vec<finalize::ArchiveEntry> = Vec::new();
            for continent in continents {
                let path = store::store_path(&cli.work, &continent);
                if !path.exists() {
                    continue;
                }
                {
                    let st = store::ChunkStore::open(&path)?;
                    if let Some(entry) = finalize::one(&st, &cli.out, p.zoom, &continent)? {
                        manifest.push(entry);
                    }
                }
                if !keep_chunks {
                    for suffix in ["", "-wal", "-shm"] {
                        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
                    }
                }
            }

            // The client reads this first and opens only the archives its bbox touches, so a
            // bbox in Hamburg never pays a request to find out south-america does not have it.
            manifest.sort_by(|a, b| a.name.cmp(&b.name));
            let index = cli.out.join("archives.json");
            std::fs::write(
                &index,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "zoom": p.zoom,
                    "format": "AOT1+zstd",
                    "attribution": "© OpenStreetMap contributors, ODbL 1.0",
                    "archives": manifest,
                }))
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            println!("{} written", index.display());
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
