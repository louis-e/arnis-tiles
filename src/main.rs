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
    /// Merge the chunk store into out/<continent>.pmtiles.
    Finalize,
    /// Print what is in the chunk store.
    Status,
    /// Decode one tile out of the chunk store (debugging).
    Inspect { x: u32, y: u32 },
}

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
            let mut st = store::ChunkStore::open(&cli.work.join("chunks.db"))?;
            let pbf_dir = cli.work.join("pbf");
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
                        download::fetch(&r.url, &pbf_dir, r.bytes)?
                    }
                };
                if let Some(next) = todo.get(i + 1) {
                    if !st.is_done(&next.id)? {
                        let (url, dir, bytes, id) =
                            (next.url.clone(), pbf_dir.clone(), next.bytes, next.id.clone());
                        prefetch = Some((
                            id,
                            std::thread::spawn(move || download::fetch(&url, &dir, bytes)),
                        ));
                    }
                }
                let t0 = std::time::Instant::now();
                let s = bake::bake(&path, &mut st, &r.id, p.zoom)?;
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

        Cmd::Finalize => {
            let p = load_plan(&cli.work)?;
            let st = store::ChunkStore::open(&cli.work.join("chunks.db"))?;
            let continent_of: HashMap<String, String> = p
                .regions
                .iter()
                .map(|r| (r.id.clone(), r.continent.clone()))
                .collect();
            finalize::run(&st, &cli.out, p.zoom, &continent_of)?;
        }

        Cmd::Status => {
            let st = store::ChunkStore::open(&cli.work.join("chunks.db"))?;
            let conn = st.connection();
            let (regions, tiles, bytes): (i64, i64, i64) = conn
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(tiles),0), COALESCE(SUM(bytes),0) FROM done",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| e.to_string())?;
            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM chunk", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            println!("{regions} regions baked, {rows} chunk rows, {tiles} tile-writes, {:.2} GB uncompressed", bytes as f64 / 1e9);
            if let Ok(p) = load_plan(&cli.work) {
                println!("plan has {} regions", p.regions.len());
            }
        }

        Cmd::Inspect { x, y } => {
            let st = store::ChunkStore::open(&cli.work.join("chunks.db"))?;
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
                let t = format::decode(&blob)?;
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
